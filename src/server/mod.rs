//! The session server: owns every pane, workspace and attached client.
//!
//! All state lives on one thread. PTY readers, process waiters and client sockets run on helper
//! threads that only forward [`Event`]s through a channel, so the state needs no locking.

mod agents;
mod fileman;
mod input;
mod render;
mod requests;
mod ssh;
mod ui;

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use ratatui::layout::Rect;

use crate::agent::{AgentState, detect_agent};
use crate::ansi::Encoder;
use crate::config::{Config, Keymap, NotifyMode};
use crate::input::InputParser;
use crate::layout::{Axis, Node, PaneId};
use crate::pane::{Pane, PaneEnv, PaneSignal, PaneSpec};
use crate::paths;
use crate::persist::{self, SessionSnapshot};
use crate::protocol::{self, ClientMsg, ServerMsg};
use crate::workspace::{self, LayoutNode, PaneKind, SizeSpec, WorkspaceDef};

pub use agents::usage_text;
pub use ui::Mode;

pub type ClientId = u32;

/// Everything the server thread reacts to.
pub enum Event {
    /// A pane has output waiting (see [`Server::pump_output`]).
    PaneReady(PaneId),
    /// Pane id, pid of the process that exited, exit code.
    PaneExited(PaneId, Option<u32>, Option<i32>),
    Connection(UnixStream),
    ClientFrame(ClientId, u8, Vec<u8>),
    ClientGone(ClientId),
    /// A background job (file listing) finished; poll and redraw.
    Wake,
    Shutdown,
}

/// A floating pane's position as fractions of the main area, so it follows resizes.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FloatRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl FloatRect {
    pub fn to_rect(self, area: Rect) -> Rect {
        let w = ((self.w * area.width as f32).round() as u16).clamp(10.min(area.width), area.width);
        let h = ((self.h * area.height as f32).round() as u16).clamp(5.min(area.height), area.height);
        let x = area.x + ((self.x * area.width as f32).round() as u16).min(area.width - w);
        let y = area.y + ((self.y * area.height as f32).round() as u16).min(area.height - h);
        Rect::new(x, y, w, h)
    }

    pub fn from_rect(r: Rect, area: Rect) -> FloatRect {
        let fw = area.width.max(1) as f32;
        let fh = area.height.max(1) as f32;
        FloatRect {
            x: (r.x - area.x) as f32 / fw,
            y: (r.y - area.y) as f32 / fh,
            w: r.width as f32 / fw,
            h: r.height as f32 / fh,
        }
    }
}

pub struct Floating {
    pub pane: PaneId,
    pub rect: FloatRect,
}

pub struct Tab {
    pub id: u32,
    pub name: String,
    pub root: Option<Node>,
    pub floating: Vec<Floating>,
    pub focus: Option<PaneId>,
    pub show_floating: bool,
    pub zoom: Option<PaneId>,
    /// Most recently focused first.
    pub recent: Vec<PaneId>,
}

impl Tab {
    pub fn new(id: u32, name: String) -> Tab {
        Tab {
            id,
            name,
            root: None,
            floating: Vec::new(),
            focus: None,
            show_floating: false,
            zoom: None,
            recent: Vec::new(),
        }
    }

    pub fn panes(&self) -> Vec<PaneId> {
        let mut v = self.root.as_ref().map(|r| r.leaves()).unwrap_or_default();
        v.extend(self.floating.iter().map(|f| f.pane));
        v
    }

    pub fn contains(&self, id: PaneId) -> bool {
        self.root.as_ref().is_some_and(|r| r.contains(id)) || self.floating.iter().any(|f| f.pane == id)
    }

    pub fn is_floating(&self, id: PaneId) -> bool {
        self.floating.iter().any(|f| f.pane == id)
    }

    pub fn set_focus(&mut self, id: PaneId) {
        self.focus = Some(id);
        self.recent.retain(|p| *p != id);
        self.recent.insert(0, id);
        if self.is_floating(id) {
            self.show_floating = true;
            // Raise it.
            if let Some(i) = self.floating.iter().position(|f| f.pane == id) {
                let f = self.floating.remove(i);
                self.floating.push(f);
            }
        } else if self.zoom.is_some_and(|z| z != id) {
            self.zoom = Some(id);
        }
    }

    /// Remove a pane; returns false if the tab is now empty.
    fn remove(&mut self, id: PaneId) -> bool {
        self.floating.retain(|f| f.pane != id);
        if let Some(root) = &mut self.root
            && root.contains(id)
            && !root.remove(id)
        {
            self.root = None;
        }
        if self.zoom == Some(id) {
            self.zoom = None;
        }
        self.recent.retain(|p| *p != id);
        if self.focus == Some(id) {
            let visible = |p: &PaneId| self.root.as_ref().is_some_and(|r| r.contains(*p)) || self.show_floating;
            self.focus = self.recent.iter().copied().find(visible).or_else(|| self.panes().first().copied());
            if let Some(f) = self.focus
                && !self.is_floating(f)
            {
                self.show_floating = self.show_floating && !self.floating.is_empty();
            }
        }
        if self.floating.is_empty() {
            self.show_floating = false;
        }
        !self.panes().is_empty()
    }
}

/// Where typed keys go besides the focused pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncScope {
    #[default]
    Off,
    /// Every terminal pane of the current tab.
    Tab,
    /// Every terminal pane of the workspace, all tabs.
    Workspace,
    /// The panes the user marked.
    Marked,
}

pub struct Workspace {
    pub id: u32,
    pub name: String,
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub definition: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub sync: SyncScope,
    /// Panes marked for typing in sync.
    pub marked: std::collections::HashSet<PaneId>,
}

impl Workspace {
    /// Panes that receive what is typed in `focused`, besides itself.
    pub fn sync_targets(&self, focused: PaneId) -> Vec<PaneId> {
        let mut out = match self.sync {
            SyncScope::Off => Vec::new(),
            SyncScope::Tab => self.tab().root.as_ref().map(|r| r.leaves()).unwrap_or_default(),
            SyncScope::Workspace => {
                self.tabs.iter().flat_map(|t| t.root.as_ref().map(|r| r.leaves()).unwrap_or_default()).collect()
            }
            SyncScope::Marked => self.panes().into_iter().filter(|p| self.marked.contains(p)).collect(),
        };
        if !out.contains(&focused) {
            out.insert(0, focused);
        }
        out
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active.min(self.tabs.len() - 1)]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        let i = self.active.min(self.tabs.len() - 1);
        &mut self.tabs[i]
    }

    pub fn panes(&self) -> Vec<PaneId> {
        self.tabs.iter().flat_map(|t| t.panes()).collect()
    }
}

pub struct Client {
    pub id: ClientId,
    pub out: Sender<Vec<u8>>,
    pub attached: bool,
    pub cols: u16,
    pub rows: u16,
    pub workspace: u32,
    pub parser: InputParser,
    pub pending_since: Option<Instant>,
    pub mode: Mode,
    pub encoder: Encoder,
    pub sidebar: bool,
    pub dirty: bool,
    pub last_input: Instant,
    pub terminal_focused: bool,
    pub drag: Option<ui::Drag>,
    pub toast: Option<(String, Instant)>,
    /// Click bookkeeping for double/triple click selection.
    pub last_click: Option<(Instant, u16, u16, u8)>,
}

/// A control request waiting for a pane condition.
struct PendingWait {
    client: ClientId,
    out: Sender<Vec<u8>>,
    pane: PaneId,
    until: Vec<String>,
    deadline: Option<Instant>,
}

pub struct Server {
    pub session: String,
    pub config: Config,
    pub keymap: Keymap,
    pub env: PaneEnv,
    pub tx: Sender<Event>,
    pub panes: HashMap<PaneId, Pane>,
    pub workspaces: Vec<Workspace>,
    pub clients: HashMap<ClientId, Client>,
    next_id: u32,
    /// The client whose terminal size drives pane sizes.
    pub primary: Option<ClientId>,
    pub layout_area: Rect,
    waits: Vec<PendingWait>,
    persist_dirty: bool,
    last_persist: Instant,
    last_tick: Instant,
    next_render: Option<Instant>,
    quitting: bool,
    /// Colours of the outer terminal, learnt from OSC 10/11 replies.
    pub outer_colors: Option<(String, String)>,
    pub warnings: Vec<String>,
    /// Names of the workspace definitions on disk, refreshed periodically.
    pub def_names: Vec<String>,
    last_def_scan: Instant,
    /// Servers from the inventory, workspaces and ~/.ssh/config.
    pub known_servers: Vec<crate::servers::Known>,
    /// ssh destinations already looked at for auto-adding.
    discovered: std::collections::HashSet<String>,
    pub accounts: crate::accounts::Accounts,
    /// Name of the workspace whose definition is being started (for account rules).
    starting_workspace: Option<String>,
    pub usage: crate::accounts::UsageMap,
    last_usage_load: Instant,
    last_codex_poll: Instant,
    limit_alerts: HashMap<PaneId, agents::LimitAlert>,
    /// Panes with output waiting, in round-robin order.
    ready: std::collections::VecDeque<PaneId>,
    ready_set: std::collections::HashSet<PaneId>,
}

const TICK: Duration = Duration::from_millis(400);
const FRAME: Duration = Duration::from_millis(16);
const INPUT_FLUSH: Duration = Duration::from_millis(30);

/// Run a session server in the foreground until it is told to exit.
pub fn run(session: &str) -> Result<()> {
    let socket = paths::socket_path(session);
    paths::private_runtime_dir().context("checking the runtime directory")?;
    if socket.exists() {
        if UnixStream::connect(&socket).is_ok() {
            anyhow::bail!("session {session:?} is already running");
        }
        let _ = std::fs::remove_file(&socket);
    }
    let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;

    let (config, mut warnings) = match Config::load() {
        Ok(c) => (c, Vec::new()),
        Err(e) => (Config::default(), vec![format!("config: {e:#}")]),
    };
    let (keymap, kw) = Keymap::build(&config.keys);
    warnings.extend(kw);
    let (theme, tw) = crate::theme::Theme::from_config(&config.theme);
    crate::theme::set(theme);
    warnings.extend(tw);

    let (tx, rx) = crossbeam_channel::bounded::<Event>(4096);
    {
        let tx = tx.clone();
        std::thread::Builder::new().name("accept".into()).spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        if tx.send(Event::Connection(s)).is_err() {
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
        })?;
    }
    {
        let tx = tx.clone();
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGHUP,
        ])?;
        std::thread::Builder::new().name("signals".into()).spawn(move || {
            if signals.forever().next().is_some() {
                let _ = tx.send(Event::Shutdown);
            }
        })?;
    }

    let env = PaneEnv {
        socket: socket.clone(),
        session: session.to_string(),
        shell: config.shell(),
        scrollback: config.scrollback,
    };
    let mut server = Server {
        session: session.to_string(),
        config,
        keymap,
        env,
        tx,
        panes: HashMap::new(),
        workspaces: Vec::new(),
        clients: HashMap::new(),
        next_id: 1,
        primary: None,
        layout_area: Rect::new(0, 1, 120, 38),
        waits: Vec::new(),
        persist_dirty: false,
        last_persist: Instant::now(),
        last_tick: Instant::now(),
        next_render: None,
        quitting: false,
        outer_colors: None,
        warnings,
        def_names: Vec::new(),
        last_def_scan: Instant::now(),
        known_servers: Vec::new(),
        discovered: Default::default(),
        accounts: crate::accounts::Accounts::empty(),
        starting_workspace: None,
        usage: Default::default(),
        last_usage_load: Instant::now(),
        last_codex_poll: Instant::now() - Duration::from_secs(3600),
        limit_alerts: HashMap::new(),
        ready: Default::default(),
        ready_set: Default::default(),
    };
    server.scan_definitions();
    server.restore();
    let result = server.event_loop(&rx);
    server.shutdown();
    let _ = std::fs::remove_file(&socket);
    result
}

impl Server {
    pub fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn event_loop(&mut self, rx: &Receiver<Event>) -> Result<()> {
        loop {
            let timeout = if self.ready.is_empty() {
                self.next_deadline().saturating_duration_since(Instant::now())
            } else {
                Duration::ZERO
            };
            match rx.recv_timeout(timeout) {
                Ok(ev) => self.handle(ev),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            // Keystrokes, clients and control requests come first…
            while let Ok(ev) = rx.try_recv() {
                self.handle(ev);
            }
            // …then a slice of program output, interrupted as soon as something else arrives.
            self.pump_output(Duration::from_millis(6), rx);
            self.timers();
            if self.quitting {
                break;
            }
            if self.next_render.is_some_and(|t| t <= Instant::now()) {
                self.render_clients();
            }
        }
        Ok(())
    }

    /// Parse waiting pane output for up to `budget`: the panes clients are looking at first,
    /// then the others in turn, one chunk each. Stops early when a control event arrives.
    fn pump_output(&mut self, budget: Duration, rx: &Receiver<Event>) {
        let start = Instant::now();
        let focused: Vec<PaneId> =
            self.clients.values().filter(|c| c.attached).filter_map(|c| self.client_focus(c.id)).collect();
        for p in focused {
            while start.elapsed() < budget && rx.is_empty() {
                let Some(data) = self.panes.get(&p).and_then(|pn| pn.take_output()) else { break };
                self.pane_output(p, &data);
            }
        }
        while start.elapsed() < budget && rx.is_empty() {
            let Some(id) = self.ready.pop_front() else { break };
            match self.panes.get(&id).and_then(|pn| pn.take_output()) {
                Some(data) => {
                    self.pane_output(id, &data);
                    self.ready.push_back(id);
                }
                None => {
                    self.ready_set.remove(&id);
                }
            }
        }
    }

    fn next_deadline(&self) -> Instant {
        let mut d = self.last_tick + TICK;
        if let Some(r) = self.next_render {
            d = d.min(r);
        }
        for c in self.clients.values() {
            if let Some(p) = c.pending_since {
                d = d.min(p + INPUT_FLUSH);
            }
            if let Some((_, until)) = &c.toast {
                d = d.min(*until);
            }
        }
        for w in &self.waits {
            if let Some(dl) = w.deadline {
                d = d.min(dl);
            }
        }
        d.max(Instant::now() + Duration::from_millis(1))
    }

    fn timers(&mut self) {
        let now = Instant::now();
        // Escape sequences that never completed.
        let flush: Vec<ClientId> = self
            .clients
            .values()
            .filter(|c| c.pending_since.is_some_and(|p| now >= p + INPUT_FLUSH))
            .map(|c| c.id)
            .collect();
        for id in flush {
            self.flush_client_input(id);
        }
        let expired: Vec<ClientId> =
            self.clients.values().filter(|c| c.toast.as_ref().is_some_and(|(_, t)| *t <= now)).map(|c| c.id).collect();
        for id in expired {
            if let Some(c) = self.clients.get_mut(&id) {
                c.toast = None;
            }
            self.mark_dirty_client(id);
        }
        let mut sync_flushed = false;
        for p in self.panes.values_mut() {
            sync_flushed |= p.poll_sync_timeout();
        }
        if sync_flushed {
            self.mark_dirty();
        }
        if now >= self.last_tick + TICK {
            self.last_tick = now;
            self.tick();
        }
        self.check_waits();
    }

    /// Periodic work: agent detection, preview reloads, persistence.
    fn tick(&mut self) {
        let mut changed = Vec::new();
        let mut ssh_seen = Vec::new();
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            let Some(pane) = self.panes.get_mut(&id) else { continue };
            if let crate::pane::Content::Preview(p) = &mut pane.content {
                if p.modified_since_load() {
                    p.reload();
                    changed.push((id, None));
                }
                continue;
            }
            if pane.exit_code.is_some() {
                continue;
            }
            let before = pane.agent.state;
            let argv = pane.foreground_argv();
            if let Some(a) = &argv
                && a.first().is_some_and(|c| c.rsplit('/').next() == Some("ssh"))
            {
                ssh_seen.push((id, a[1..].to_vec()));
            }
            let agent = argv.as_deref().and_then(detect_agent);
            let mut moved = pane.agent.set_process(agent);
            if pane.agent.kind.is_some() {
                let screen = pane.text(None);
                let quiet = pane.last_output.elapsed();
                moved |= pane.agent.observe(&screen, quiet);
            }
            if moved {
                changed.push((id, Some(before)));
            }
        }
        for (id, before) in changed {
            if let Some(before) = before {
                self.on_agent_change(id, before);
            }
            self.mark_dirty();
        }
        for (id, args) in ssh_seen {
            self.observe_ssh(id, &args);
        }
        // Wake-ups can be dropped when the event queue is full; catch those panes here.
        let waiting: Vec<PaneId> = self.panes.values().filter(|p| p.has_output()).map(|p| p.id).collect();
        for id in waiting {
            if self.ready_set.insert(id) {
                self.ready.push_back(id);
            }
        }
        // Leave copy mode in panes no client is browsing any more (focus moved, client gone).
        let browsing: Vec<PaneId> = self
            .clients
            .values()
            .filter(|c| matches!(c.mode, Mode::Scroll | Mode::Search { .. }))
            .filter_map(|c| self.client_focus(c.id))
            .collect();
        for p in self.panes.values_mut() {
            if p.in_copy_mode() && !browsing.contains(&p.id) {
                p.copy_mode(false);
            }
        }
        if self.last_usage_load.elapsed() > Duration::from_secs(3) {
            self.reload_usage();
            self.check_limits();
        }
        if self.last_codex_poll.elapsed() > Duration::from_secs(300) {
            self.poll_codex_usage();
        }
        let expired: Vec<PaneId> =
            self.panes.values().filter(|p| p.close_at.is_some_and(|t| t <= Instant::now())).map(|p| p.id).collect();
        for id in expired {
            self.close_pane(id);
        }
        if self.persist_dirty || self.last_persist.elapsed() > Duration::from_secs(30) {
            self.persist();
        }
        if self.last_def_scan.elapsed() > Duration::from_secs(5) {
            self.scan_definitions();
        }
    }

    fn scan_definitions(&mut self) {
        self.last_def_scan = Instant::now();
        let defs: Vec<WorkspaceDef> = self.definitions().into_iter().filter_map(|(_, d)| d.ok()).collect();
        let names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
        if names != self.def_names {
            self.def_names = names;
            self.mark_dirty();
        }
        self.refresh_known(&defs);
        self.reload_accounts();
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::PaneReady(id) => {
                if self.ready_set.insert(id) {
                    self.ready.push_back(id);
                }
            }
            Event::PaneExited(id, pid, code) => self.pane_exited(id, pid, code),
            Event::Connection(stream) => self.new_connection(stream),
            Event::ClientFrame(id, kind, payload) => self.client_frame(id, kind, payload),
            Event::ClientGone(id) => self.drop_client(id),
            Event::Wake => {
                let mut changed = false;
                for p in self.panes.values_mut() {
                    if let crate::pane::Content::Files(f) = &mut p.content {
                        changed |= f.poll();
                    }
                }
                if changed {
                    self.mark_dirty();
                }
            }
            Event::Shutdown => self.quitting = true,
        }
    }

    fn pane_output(&mut self, id: PaneId, data: &[u8]) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let signals = pane.feed(data);
        for s in signals {
            match s {
                PaneSignal::Notification { title, body } => {
                    let title = if title.is_empty() { self.pane_label(id) } else { title };
                    self.notify(&title, &body, Some(id));
                }
                PaneSignal::Clipboard(text) => {
                    let bytes = crate::pane::osc52(&text);
                    for c in self.clients.values().filter(|c| c.attached) {
                        let _ = c.out.try_send(frame_bytes(&bytes));
                    }
                }
                PaneSignal::Bell | PaneSignal::TitleChanged => self.persist_dirty |= false,
            }
        }
        self.mark_dirty_pane(id);
    }

    fn pane_exited(&mut self, id: PaneId, pid: Option<u32>, code: Option<i32>) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if pid.is_some() && pane.pid() != pid {
            return; // an earlier process of a respawned pane
        }
        if let Some(spec) = pane.restart_with.take() {
            pane.spec = spec;
            if let Err(e) = pane.respawn(&self.env, &self.tx) {
                self.warnings.push(format!("{e:#}"));
            }
            self.limit_alerts.remove(&id);
            self.relayout();
            return;
        }
        let keep = pane.suspended || (!pane.spec.command.is_empty() && !pane.spec.close_on_exit);
        if keep {
            pane.exit_code = Some(code);
            pane.agent = Default::default();
            let job = pane.spec.close_on_success;
            if code == Some(0) && job {
                pane.close_at = Some(Instant::now() + Duration::from_secs(3));
            }
            if job {
                self.refresh_file_managers();
            }
            self.mark_dirty();
            self.check_waits();
        } else {
            self.close_pane(id);
        }
    }

    // ---- connections -------------------------------------------------------------------

    fn new_connection(&mut self, stream: UnixStream) {
        let id = self.alloc_id();
        let Ok(read_half) = stream.try_clone() else { return };
        let (out_tx, out_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);
        let mut write_half = stream;
        let _ = std::thread::Builder::new().name(format!("client-{id}-write")).spawn(move || {
            for data in out_rx {
                if write_half.write_all(&data).and_then(|_| write_half.flush()).is_err() {
                    break;
                }
            }
            let _ = write_half.shutdown(std::net::Shutdown::Both);
        });
        let tx = self.tx.clone();
        let _ = std::thread::Builder::new().name(format!("client-{id}-read")).spawn(move || {
            let mut r = std::io::BufReader::with_capacity(64 * 1024, read_half);
            while let Ok(Some((kind, payload))) = protocol::read_frame(&mut r) {
                if tx.send(Event::ClientFrame(id, kind, payload)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::ClientGone(id));
        });
        let ws = self.workspaces.first().map(|w| w.id).unwrap_or(0);
        self.clients.insert(
            id,
            Client {
                id,
                out: out_tx,
                attached: false,
                cols: 80,
                rows: 24,
                workspace: ws,
                parser: InputParser::new(),
                pending_since: None,
                mode: Mode::Normal,
                encoder: Encoder::new(),
                sidebar: self.config.sidebar.visible,
                dirty: false,
                last_input: Instant::now(),
                terminal_focused: true,
                drag: None,
                toast: None,
                last_click: None,
            },
        );
    }

    fn client_frame(&mut self, id: ClientId, kind: u8, payload: Vec<u8>) {
        if kind == protocol::BYTES {
            self.client_input(id, &payload);
            return;
        }
        let msg: ClientMsg = match serde_json::from_slice(&payload) {
            Ok(m) => m,
            Err(e) => {
                self.send_msg(id, &ServerMsg::Response { result: Err(format!("bad message: {e}")) });
                return;
            }
        };
        match msg {
            ClientMsg::Attach { version, cols, rows, workspace } => {
                if version != protocol::PROTOCOL_VERSION {
                    self.send_msg(
                        id,
                        &ServerMsg::Exit {
                            reason: format!(
                                "protocol mismatch: server v{}, client v{version}; restart the server",
                                protocol::PROTOCOL_VERSION
                            ),
                        },
                    );
                    return;
                }
                self.attach(id, cols, rows, workspace);
            }
            ClientMsg::Resize { cols, rows } => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.cols = cols;
                    c.rows = rows;
                    c.encoder.invalidate();
                }
                self.primary = Some(id);
                self.relayout();
            }
            ClientMsg::Detach => self.detach(id, "detached"),
            ClientMsg::Request { version, request } => {
                let result = if version != protocol::PROTOCOL_VERSION {
                    Some(Err(format!("protocol mismatch: server v{}, client v{version}", protocol::PROTOCOL_VERSION)))
                } else {
                    self.request(id, request)
                };
                if let Some(result) = result {
                    self.send_msg(id, &ServerMsg::Response { result });
                    self.clients.remove(&id);
                }
            }
        }
    }

    fn attach(&mut self, id: ClientId, cols: u16, rows: u16, workspace: Option<String>) {
        let mut target = None;
        if let Some(name) = workspace {
            match self.open_workspace(&name) {
                Ok(ws) => target = Some(ws),
                Err(e) => self.warnings.push(format!("{e:#}")),
            }
        }
        if self.workspaces.is_empty() {
            let ws = self.new_workspace("main".into(), None);
            if let Err(e) = self.spawn_pane_in(ws, None, PaneSpec::default(), crate::protocol::Placement::Tab, None) {
                self.send_msg(id, &ServerMsg::Exit { reason: format!("cannot start a shell: {e:#}") });
                return;
            }
        }
        let ws = target.unwrap_or_else(|| self.workspaces[0].id);
        let warnings = std::mem::take(&mut self.warnings);
        if let Some(c) = self.clients.get_mut(&id) {
            c.attached = true;
            c.cols = cols;
            c.rows = rows;
            c.workspace = ws;
            c.dirty = true;
            if !warnings.is_empty() {
                c.toast = Some((warnings.join(" · "), Instant::now() + Duration::from_secs(8)));
            }
            // Ask the outer terminal for its colours so panes querying them get real values.
            let _ = c.out.try_send(frame_bytes(b"\x1b]10;?\x07\x1b]11;?\x07"));
        }
        self.primary = Some(id);
        self.relayout();
        self.mark_seen_focused(id);
        self.mark_dirty();
    }

    pub fn detach(&mut self, id: ClientId, reason: &str) {
        self.send_msg(id, &ServerMsg::Exit { reason: reason.to_string() });
        self.drop_client(id);
    }

    fn drop_client(&mut self, id: ClientId) {
        self.clients.remove(&id);
        // A `kraal wait` whose caller went away is dropped with it.
        self.waits.retain(|w| w.client != id);
        if self.primary == Some(id) {
            self.primary = self.clients.values().filter(|c| c.attached).max_by_key(|c| c.last_input).map(|c| c.id);
            self.relayout();
        }
    }

    pub fn send_msg(&self, id: ClientId, msg: &ServerMsg) {
        if let Some(c) = self.clients.get(&id) {
            let mut buf = Vec::new();
            if protocol::write_json(&mut buf, msg).is_ok() {
                let _ = c.out.send_timeout(buf, Duration::from_millis(200));
            }
        }
    }

    // ---- rendering ---------------------------------------------------------------------

    pub fn mark_dirty(&mut self) {
        for c in self.clients.values_mut() {
            c.dirty = true;
        }
        self.schedule_render();
    }

    pub fn mark_dirty_client(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.dirty = true;
        }
        self.schedule_render();
    }

    fn mark_dirty_pane(&mut self, pane: PaneId) {
        let ws = self.workspace_of(pane).map(|(w, _)| self.workspaces[w].id);
        for c in self.clients.values_mut() {
            if Some(c.workspace) == ws {
                c.dirty = true;
            }
        }
        self.schedule_render();
    }

    fn schedule_render(&mut self) {
        if self.next_render.is_none() {
            self.next_render = Some(Instant::now() + Duration::from_millis(2));
        }
    }

    fn render_clients(&mut self) {
        self.next_render = None;
        let ids: Vec<ClientId> = self.clients.values().filter(|c| c.attached && c.dirty).map(|c| c.id).collect();
        let mut again = false;
        for id in ids {
            // Slow client: keep it dirty and retry on the next frame.
            if self.clients[&id].out.len() >= 8 {
                again = true;
                continue;
            }
            let (buf, cursor, links) = self.compose(id);
            let Some(c) = self.clients.get_mut(&id) else { continue };
            let mut bytes = Vec::with_capacity(16 * 1024);
            c.encoder.encode(&buf, cursor, &links, &mut bytes);
            c.dirty = false;
            match c.out.try_send(frame_bytes(&bytes)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    c.encoder.invalidate();
                    c.dirty = true;
                    again = true;
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
        if again {
            self.next_render = Some(Instant::now() + FRAME);
        }
    }

    // ---- geometry ----------------------------------------------------------------------

    /// Main pane area for a client of the given size.
    pub fn main_area(&self, cols: u16, rows: u16, sidebar: bool) -> Rect {
        let top = 1;
        let bottom = 1;
        let height = rows.saturating_sub(top + bottom).max(1);
        let sw = if sidebar { self.sidebar_width(cols) } else { 0 };
        Rect::new(sw, top, cols.saturating_sub(sw).max(1), height)
    }

    pub fn sidebar_width(&self, cols: u16) -> u16 {
        let w = self.config.sidebar.width;
        if cols < w * 3 { 0 } else { w }
    }

    /// Outer rectangle of every visible pane of a tab, in draw order (tiled, then floating).
    pub fn tab_rects(&self, tab: &Tab, area: Rect) -> Vec<(PaneId, Rect, bool)> {
        let mut out = Vec::new();
        if let Some(z) = tab.zoom.filter(|z| !tab.is_floating(*z)) {
            out.push((z, area, false));
        } else if let Some(root) = &tab.root {
            let rects = root.layout(area);
            for id in root.leaves() {
                out.push((id, rects[&id], false));
            }
        }
        if tab.show_floating {
            for f in &tab.floating {
                out.push((f.pane, f.rect.to_rect(area), true));
            }
        }
        out
    }

    /// Inner (content) rectangle of a pane drawn at `outer`.
    pub fn inner(&self, outer: Rect) -> Rect {
        if self.config.pane_frames && outer.width >= 4 && outer.height >= 3 {
            Rect::new(outer.x + 1, outer.y + 1, outer.width - 2, outer.height - 2)
        } else {
            outer
        }
    }

    /// Recompute the layout area from the primary client and resize every pane.
    pub fn relayout(&mut self) {
        if let Some(c) = self.primary.and_then(|p| self.clients.get(&p)) {
            self.layout_area = self.main_area(c.cols, c.rows, c.sidebar);
        }
        let area = self.layout_area;
        let mut sizes = Vec::new();
        for ws in &self.workspaces {
            for tab in &ws.tabs {
                let mut tiled = tab.root.as_ref().map(|r| r.layout(area)).unwrap_or_default();
                if let Some(z) = tab.zoom {
                    tiled.insert(z, area);
                }
                for (id, r) in tiled {
                    sizes.push((id, self.inner(r)));
                }
                for f in &tab.floating {
                    sizes.push((f.pane, self.inner(f.rect.to_rect(area))));
                }
            }
        }
        for (id, r) in sizes {
            if let Some(p) = self.panes.get_mut(&id) {
                p.resize(r.width, r.height);
            }
        }
        for c in self.clients.values_mut() {
            c.dirty = true;
        }
        self.schedule_render();
    }

    // ---- workspaces, tabs, panes -------------------------------------------------------

    pub fn ws_index(&self, id: u32) -> Option<usize> {
        self.workspaces.iter().position(|w| w.id == id)
    }

    /// (workspace index, tab index) holding a pane.
    pub fn workspace_of(&self, pane: PaneId) -> Option<(usize, usize)> {
        for (wi, ws) in self.workspaces.iter().enumerate() {
            for (ti, tab) in ws.tabs.iter().enumerate() {
                if tab.contains(pane) {
                    return Some((wi, ti));
                }
            }
        }
        None
    }

    pub fn new_workspace(&mut self, name: String, definition: Option<PathBuf>) -> u32 {
        let id = self.alloc_id();
        let mut name = name;
        let mut n = 2;
        let base = name.clone();
        while self.workspaces.iter().any(|w| w.name == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        self.workspaces.push(Workspace {
            id,
            name,
            tabs: Vec::new(),
            active: 0,
            definition,
            cwd: None,
            sync: SyncScope::Off,
            marked: Default::default(),
        });
        self.persist_dirty = true;
        id
    }

    /// Spawn a pane process with a provisional size; call [`Server::relayout`] afterwards.
    pub fn spawn_pane(&mut self, name: Option<String>, spec: PaneSpec) -> Result<PaneId> {
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
            // A command that cannot start still gets its pane, so the layout stays whole and
            // Enter can retry once the problem is fixed.
            Err(e) if !spec.command.is_empty() => {
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
        let id = self.alloc_id();
        let inner = self.inner(self.layout_area);
        let line = crate::transfer::sh_quote(&spec.command.join(" "));
        let placeholder = PaneSpec {
            command: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "printf '\\033[2m%s\\n\\033[0m$ %s\\n' {} {line}",
                    crate::transfer::sh_quote(&format!("kraal: {reason}"))
                ),
            ],
            cwd: spec.cwd.clone().filter(|c| c.is_dir()),
            ..Default::default()
        };
        let mut pane = Pane::spawn(id, name, placeholder, inner.width, inner.height, &self.env, &self.tx)?;
        pane.ssh = self.ssh_target_of(&spec);
        pane.spec = spec;
        pane.suspended = true;
        self.panes.insert(id, pane);
        self.persist_dirty = true;
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
            let n = ws.tabs.len() + 1;
            ws.tabs.push(Tab::new(tab_id, format!("tab {n}")));
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
                            let r = rects.get(&t).copied().unwrap_or(area);
                            // Cells are about twice as tall as wide.
                            if r.width >= r.height * 2 { Axis::Row } else { Axis::Column }
                        }
                    };
                    root.split(t, axis, id);
                }
                (Some(root), None) => {
                    let first = root.leaves()[0];
                    root.split(first, Axis::Row, id);
                }
            }
            tab.zoom = None;
        }
        tab.set_focus(id);
        self.relayout();
        Ok(id)
    }

    /// Close a pane and clean up empty tabs and workspaces.
    pub fn close_pane(&mut self, id: PaneId) {
        let Some(mut pane) = self.panes.remove(&id) else { return };
        pane.kill();
        if let Some((wi, ti)) = self.workspace_of(id) {
            let ws = &mut self.workspaces[wi];
            ws.marked.remove(&id);
            if ws.sync == SyncScope::Marked && ws.marked.is_empty() {
                ws.sync = SyncScope::Off;
            }
            if !ws.tabs[ti].remove(id) {
                ws.tabs.remove(ti);
                if ws.active >= ws.tabs.len() && ws.active > 0 {
                    ws.active -= 1;
                }
            }
            if ws.tabs.is_empty() {
                let gone = ws.id;
                self.workspaces.remove(wi);
                let fallback = self.workspaces.get(wi.saturating_sub(1)).or(self.workspaces.first()).map(|w| w.id);
                for c in self.clients.values_mut() {
                    if c.workspace == gone {
                        c.workspace = fallback.unwrap_or(0);
                    }
                }
            }
        }
        self.persist_dirty = true;
        if self.workspaces.is_empty() {
            let ids: Vec<ClientId> = self.clients.keys().copied().collect();
            for c in ids {
                self.send_msg(c, &ServerMsg::Exit { reason: "all panes closed".into() });
            }
            let _ = std::fs::remove_file(paths::session_state_path(&self.session));
            self.persist_dirty = false;
            self.quitting = true;
            return;
        }
        self.check_waits();
        self.relayout();
    }

    pub fn close_workspace(&mut self, ws: u32) {
        let Some(wi) = self.ws_index(ws) else { return };
        for id in self.workspaces[wi].panes() {
            self.close_pane(id);
        }
    }

    /// Open a workspace by name: switch to it if it is running, else start it from its definition.
    pub fn open_workspace(&mut self, name: &str) -> Result<u32> {
        if let Some(ws) = self.workspaces.iter().find(|w| w.name == name) {
            return Ok(ws.id);
        }
        let (path, def) = self.find_definition(name).with_context(|| {
            let dirs: Vec<String> = self.config.workspace_dirs().iter().map(|d| paths::shorten_home(d)).collect();
            format!("no workspace named {name:?} in {}", dirs.join(", "))
        })?;
        self.start_definition(&def, Some(path))
    }

    pub fn definitions(&self) -> Vec<(PathBuf, Result<WorkspaceDef>)> {
        let mut out = Vec::new();
        for dir in self.config.workspace_dirs() {
            out.extend(workspace::load_dir(&dir));
        }
        out
    }

    fn find_definition(&self, name: &str) -> Option<(PathBuf, WorkspaceDef)> {
        self.definitions().into_iter().find_map(|(p, d)| match d {
            Ok(d) if d.name == name => Some((p, d)),
            _ => None,
        })
    }

    /// Spawn every pane of a definition into a new workspace.
    pub fn start_definition(&mut self, def: &WorkspaceDef, path: Option<PathBuf>) -> Result<u32> {
        self.starting_workspace = Some(def.name.clone());
        let before: std::collections::HashSet<PaneId> = self.panes.keys().copied().collect();
        let workspaces_before: Vec<u32> = self.workspaces.iter().map(|w| w.id).collect();
        let result = self.start_definition_inner(def, path);
        self.starting_workspace = None;
        if result.is_err() {
            self.rollback(&before, &workspaces_before);
        }
        result
    }

    fn start_definition_inner(&mut self, def: &WorkspaceDef, path: Option<PathBuf>) -> Result<u32> {
        let ws = self.new_workspace(def.name.clone(), path);
        let wi = self.ws_index(ws).unwrap();
        self.workspaces[wi].cwd = def.cwd.clone();
        let mut focus_tab = 0;
        for (ti, tdef) in def.tabs.iter().enumerate() {
            let tab_id = self.alloc_id();
            let mut tab = Tab::new(tab_id, tdef.name.clone().unwrap_or_else(|| format!("tab {}", ti + 1)));
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
            let first = tab.panes().first().copied();
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
                let alive: Vec<PaneId> = tab.panes().into_iter().filter(|p| !self.panes.contains_key(p)).collect();
                for p in alive {
                    tab.remove(p);
                }
            }
            ws.tabs.retain(|t| !t.panes().is_empty());
        }
        self.workspaces.retain(|w| !w.tabs.is_empty());
        self.relayout();
    }

    fn build_node(
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

    fn spawn_from_def(&mut self, def: &workspace::PaneDef, cwd: Option<&PathBuf>) -> Result<PaneId> {
        let mut spec = PaneSpec {
            cwd: def.cwd.clone().or_else(|| cwd.cloned()),
            close_on_exit: def.close_on_exit,
            ..Default::default()
        };
        match &def.kind {
            PaneKind::Preview(p) => {
                let p = if p.is_relative() { spec.cwd.clone().unwrap_or_default().join(p) } else { p.clone() };
                spec.preview = Some(p);
            }
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
                        let a = match account {
                            Some(name) => self.accounts.get(kind, name),
                            None => self.accounts.choose(
                                kind,
                                ws.as_deref(),
                                spec.cwd.as_deref(),
                                &self.usage,
                                crate::accounts::now(),
                            ),
                        };
                        if let Some(a) = a {
                            let mut agent_spec = self.agent_spec(&a, spec.cwd.clone(), None, &def.args)?;
                            agent_spec.close_on_exit = spec.close_on_exit;
                            spec = agent_spec;
                        }
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

    /// Current layout of a workspace as a definition (for "save workspace").
    pub fn snapshot_definition(&self, wi: usize, name: &str) -> WorkspaceDef {
        let ws = &self.workspaces[wi];
        let to_def = |id: PaneId, focused: bool| -> workspace::PaneDef {
            let p = &self.panes[&id];
            let (kind, command, args) = match &p.spec.preview {
                Some(path) => (PaneKind::Preview(path.clone()), None, Vec::new()),
                None => {
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
                agent: if p.spec.command.is_empty() { p.agent.kind.clone() } else { None },
                start_suspended: false,
                close_on_exit: p.spec.close_on_exit,
            }
        };
        fn conv(n: &Node, focus: Option<PaneId>, to_def: &dyn Fn(PaneId, bool) -> workspace::PaneDef) -> LayoutNode {
            match n {
                Node::Leaf(id) => LayoutNode::Pane(to_def(*id, focus == Some(*id))),
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
            .filter_map(|(i, t)| {
                let root = t.root.as_ref()?;
                Some(workspace::TabDef {
                    name: Some(t.name.clone()),
                    focus: i == ws.active,
                    cwd: None,
                    root: conv(root, t.focus, &to_def),
                    floating: t.floating.iter().map(|f| to_def(f.pane, t.focus == Some(f.pane))).collect(),
                })
            })
            .collect();
        WorkspaceDef { name: name.to_string(), cwd: ws.cwd.clone(), tabs, warnings: Vec::new() }
    }

    // ---- focus & attention -------------------------------------------------------------

    /// Focused pane of the tab a client is looking at.
    pub fn client_focus(&self, id: ClientId) -> Option<PaneId> {
        let c = self.clients.get(&id)?;
        let wi = self.ws_index(c.workspace)?;
        self.workspaces[wi].tab().focus
    }

    /// Make a pane visible and focused for a client.
    pub fn reveal(&mut self, client: ClientId, pane: PaneId) {
        let Some((wi, ti)) = self.workspace_of(pane) else { return };
        let ws = &mut self.workspaces[wi];
        ws.active = ti;
        ws.tabs[ti].set_focus(pane);
        let wid = ws.id;
        if let Some(c) = self.clients.get_mut(&client) {
            c.workspace = wid;
        }
        self.mark_seen_focused(client);
        self.mark_dirty();
    }

    /// Done agents in the client's focused pane become idle: the user has seen them.
    pub fn mark_seen_focused(&mut self, client: ClientId) {
        if let Some(p) = self.client_focus(client)
            && let Some(pane) = self.panes.get_mut(&p)
            && pane.agent.mark_seen()
        {
            self.mark_dirty();
        }
    }

    /// Panes needing attention, blocked first, oldest first.
    pub fn attention_queue(&self) -> Vec<PaneId> {
        let mut v: Vec<&Pane> = self.panes.values().filter(|p| p.agent.state.needs_attention()).collect();
        v.sort_by_key(|p| (p.agent.state != AgentState::Blocked, p.agent.since));
        v.into_iter().map(|p| p.id).collect()
    }

    /// Whether some attached client is looking at this pane right now.
    fn pane_watched(&self, pane: PaneId) -> bool {
        self.clients.values().any(|c| {
            c.attached
                && c.terminal_focused
                && self.client_focus(c.id) == Some(pane)
                && c.last_input.elapsed() < Duration::from_secs(120)
        })
    }

    pub fn on_agent_change(&mut self, pane: PaneId, before: AgentState) {
        let Some(p) = self.panes.get(&pane) else { return };
        let state = p.agent.state;
        if state == before {
            return;
        }
        if self.pane_watched(pane) {
            if state == AgentState::Done
                && let Some(p) = self.panes.get_mut(&pane)
            {
                p.agent.mark_seen();
            }
            self.check_waits();
            return;
        }
        let label = self.pane_label(pane);
        let agent = p.agent.kind.clone().unwrap_or_else(|| "agent".into());
        match state {
            AgentState::Blocked if self.config.notifications.on_blocked => {
                let msg = p.agent.message.clone().unwrap_or_else(|| "needs your input".into());
                self.notify(&format!("{agent} · {label}"), &msg, Some(pane));
            }
            AgentState::Done if self.config.notifications.on_done && before == AgentState::Working => {
                self.notify(&format!("{agent} · {label}"), "finished", Some(pane));
            }
            _ => {}
        }
        self.check_waits();
    }

    pub fn pane_label(&self, pane: PaneId) -> String {
        let name = self.panes.get(&pane).map(|p| p.display_name()).unwrap_or_default();
        match self.workspace_of(pane) {
            Some((wi, _)) => format!("{} › {name}", self.workspaces[wi].name),
            None => name,
        }
    }

    /// Desktop notification through the attached terminals and/or notify-send.
    pub fn notify(&mut self, title: &str, body: &str, _pane: Option<PaneId>) {
        let mode = self.config.notifications.mode;
        if matches!(mode, NotifyMode::Terminal | NotifyMode::Both) {
            let seq = crate::pane::osc_notify(title, body);
            for c in self.clients.values().filter(|c| c.attached) {
                let _ = c.out.try_send(frame_bytes(&seq));
            }
        }
        if matches!(mode, NotifyMode::System | NotifyMode::Both) {
            let _ = std::process::Command::new("notify-send")
                .args(["--app-name=kraal", title, body])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        for c in self.clients.values_mut().filter(|c| c.attached) {
            c.toast = Some((format!("{title}: {body}"), Instant::now() + Duration::from_secs(4)));
            c.dirty = true;
        }
        self.schedule_render();
    }

    pub fn toast(&mut self, client: ClientId, msg: impl Into<String>) {
        if let Some(c) = self.clients.get_mut(&client) {
            c.toast = Some((msg.into(), Instant::now() + Duration::from_secs(3)));
            c.dirty = true;
        }
        self.schedule_render();
    }

    // ---- waits -------------------------------------------------------------------------

    fn add_wait(&mut self, client: ClientId, pane: PaneId, until: Vec<String>, timeout: Option<Duration>) {
        let Some(c) = self.clients.remove(&client) else { return };
        self.waits.push(PendingWait { client, out: c.out, pane, until, deadline: timeout.map(|t| Instant::now() + t) });
        self.check_waits();
    }

    fn check_waits(&mut self) {
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
                        Some(Ok(protocol::Response::Pane(self.pane_info(w.pane))))
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

    // ---- persistence -------------------------------------------------------------------

    fn persist(&mut self) {
        self.persist_dirty = false;
        self.last_persist = Instant::now();
        if self.workspaces.is_empty() {
            return;
        }
        let snap = persist::snapshot(self);
        if let Err(e) = persist::save(&self.session, &snap) {
            self.warnings.push(format!("saving session state: {e:#}"));
        }
    }

    /// Recreate the layout saved by a previous server, if any.
    fn restore(&mut self) {
        let snap = match persist::load(&self.session) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                // Keep the unreadable file aside rather than overwrite it with this new session.
                let path = paths::session_state_path(&self.session);
                let aside = path.with_extension(format!("json.unreadable-{}", crate::accounts::now()));
                let _ = std::fs::rename(&path, &aside);
                self.warnings.push(format!(
                    "could not restore the previous session ({e:#}); kept it as {}",
                    paths::shorten_home(&aside)
                ));
                return;
            }
        };
        if let Err(e) = self.apply_snapshot(snap) {
            self.warnings.push(format!("restore: {e:#}"));
        }
    }

    fn apply_snapshot(&mut self, snap: SessionSnapshot) -> Result<()> {
        persist::apply(self, snap)
    }

    fn shutdown(&mut self) {
        if !self.workspaces.is_empty() {
            self.persist();
        }
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for id in ids {
            self.send_msg(id, &ServerMsg::Exit { reason: "server exited".into() });
        }
        for p in self.panes.values_mut() {
            p.kill();
        }
        // Give writer threads a moment to deliver the exit messages.
        std::thread::sleep(Duration::from_millis(50));
    }

    pub fn mark_persist(&mut self) {
        self.persist_dirty = true;
    }

    pub fn pane_info(&self, id: PaneId) -> protocol::PaneInfo {
        let p = &self.panes[&id];
        let (ws, tab, floating, focused) = match self.workspace_of(id) {
            Some((wi, ti)) => {
                let w = &self.workspaces[wi];
                let t = &w.tabs[ti];
                (w.name.clone(), t.name.clone(), t.is_floating(id), t.focus == Some(id) && w.active == ti)
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
            focused,
            floating,
            cols: p.cols,
            rows: p.rows,
        }
    }

    pub fn definition_dir(&self) -> PathBuf {
        self.config.workspace_dirs().into_iter().next().unwrap_or_else(paths::workspaces_dir)
    }
}

/// Wrap terminal bytes in a protocol frame.
pub fn frame_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 5);
    let _ = protocol::write_frame(&mut out, protocol::BYTES, bytes);
    out
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
