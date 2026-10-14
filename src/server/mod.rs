//! The session server: owns every pane, workspace and attached client.
//!
//! All state lives on one thread. PTY readers, process waiters and client sockets run on helper
//! threads that only forward [`Event`]s through a channel, so the state needs no locking.

mod account_ui;
mod agents;
mod attention;
mod bottom_bar;
mod buffers;
mod clients;
mod commands;
mod contexts;
mod definitions;
mod dirwatch;
mod edits;
pub mod events;
mod fileman;
mod forms;
mod geometry;
mod graphics;
mod hints;
mod input;
pub mod net;
mod overlays;
mod pane_ui;
mod panes;
mod persistence;
#[cfg(test)]
mod publication_tests;
mod queue;
mod reload;
pub mod remote;
mod render;
mod requests;
pub mod runbooks;
mod ssh;
mod tools;
mod ui;
mod workspace_files;
mod workspace_lifecycle;
mod workspace_order;
mod workspace_ui;
mod worktrees;

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Sender, TrySendError};
use ratatui::layout::Rect;

use crate::agent::detect_agent;
use crate::ansi::Encoder;
use crate::config::Config;
use crate::input::InputParser;
use crate::keymap::Keymap;
use crate::layout::{Node, PaneId};
use crate::pane::{Pane, PaneEnv, PaneSignal};
use crate::paths;
use crate::persist;
use crate::protocol::ServerMsg;

pub use agents::usage_text;
pub use clients::frame_bytes;
use persistence::spawn_persister;
use queue::Wait;
#[cfg(test)]
pub(crate) use queue::channel as event_channel;
pub use queue::{EventReceiver, EventSender};
pub use ssh::ssh_config_hosts;
pub use ui::Mode;

pub type ClientId = u32;

/// Everything the server thread reacts to.
pub enum Event {
    /// A pane has output waiting (see [`Server::pump_output`]).
    PaneReady(PaneId),
    /// Pane id, pid of the process that exited, exit code.
    PaneExited(PaneId, Option<u32>, Option<i32>),
    /// The program of a pane (id, process), started after the pane was set up, could not
    /// start: why.
    PaneFailed(PaneId, crate::pane::ProcessHold, String),
    Connection(UnixStream),
    ClientFrame(ClientId, u8, Vec<u8>),
    ClientGone(ClientId),
    /// A background job (file listing) finished; poll and redraw.
    Wake,
    /// `git status` of a directory, from the git thread.
    Git(PathBuf, Option<Box<crate::git::GitInfo>>),
    /// Something a helper thread wants the user to know.
    Warning(String),
    /// Publish an event once what it describes is on disk (a run's log).
    Emit(&'static str, serde_json::Value),
    /// A job started with [`Server::background`] finished with this result, or panicked.
    Job(u32, std::thread::Result<Box<dyn std::any::Any + Send>>),
    /// The agents a server's session has now, from the connection with this id.
    RemoteAgents(u32, Vec<remote::RemoteAgent>),
    /// Watched directories changed (see [`crate::watch`]).
    Watched(Vec<crate::watch::Change>),
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
    /// The arrangement the tiled panes were put in, kept as panes come and go until they are
    /// resized, split or stacked by hand.
    pub preset: Option<crate::layout::Preset>,
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
            preset: None,
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
        } else {
            self.expand_focus();
            if self.zoom.is_some_and(|z| z != id) {
                self.zoom = Some(id);
            }
        }
    }

    /// Open the focused pane in its stack, if it is folded in one.
    fn expand_focus(&mut self) {
        if let (Some(f), Some(root)) = (self.focus, &mut self.root) {
            root.expand(f);
        }
    }

    /// Put the tiled panes in `preset`, in their order, the focus kept.
    pub fn arrange(&mut self, preset: crate::layout::Preset) {
        if let Some(root) = &self.root
            && let Some(tree) = preset.build(&root.leaves(), self.focus)
        {
            self.root = Some(tree);
        }
        self.preset = Some(preset);
    }

    /// Remove a pane; returns false if the tab is now empty.
    fn remove(&mut self, id: PaneId) -> bool {
        self.floating.retain(|f| f.pane != id);
        if let Some(root) = &mut self.root
            && root.contains(id)
        {
            if !root.remove(id) {
                self.root = None;
            } else if let Some(preset) = self.preset {
                self.arrange(preset);
            }
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
            self.expand_focus();
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
    /// Saved identity; the numeric id only addresses this server's objects.
    pub stable_id: String,
    pub name: String,
    pub tabs: Vec<Tab>,
    /// The layout retained while closing or closed, independent of the remaining processes.
    pub kept: Option<persist::WorkspaceSnap>,
    pub close_error: Option<String>,
    pub delete_pending: bool,
    pub active: usize,
    /// The definition file it is linked to (see [`Workspace::set_definition`]): the one it was
    /// started from, or first saved to.
    pub definition: Option<PathBuf>,
    /// The device and inode of that file when last seen: what finds it again once renamed.
    pub definition_inode: Option<(u64, u64)>,
    /// The exact KDL loaded or last saved, independent of later directory scans.
    pub definition_source: Option<String>,
    pub cwd: Option<PathBuf>,
    pub sync: SyncScope,
    /// Panes marked for typing in sync.
    pub marked: std::collections::HashSet<PaneId>,
    /// Production panes the user agreed to type into in sync, until sync stops.
    /// The approval holds for the production reason it was given for: a pane that moves to
    /// another production server is asked about again.
    pub sync_approved: HashMap<PaneId, contexts::Production>,
}

impl Workspace {
    /// Link it to the definition file at `path`, or to none. The path is spelled as
    /// [`crate::workspace::load_dir`] lists definitions, so that they compare equal.
    pub fn set_definition(&mut self, path: Option<&Path>) {
        self.definition = path.map(crate::workspace::definition_path);
        self.definition_inode = path.and_then(crate::paths::file_id);
        if path.is_none() {
            self.definition_source = None;
        }
    }

    /// Panes that receive what is typed in `focused`, besides itself.
    pub fn sync_targets(&self, focused: PaneId) -> Vec<PaneId> {
        let mut out = match self.sync {
            SyncScope::Off => Vec::new(),
            SyncScope::Tab => self.tab().and_then(|t| t.root.as_ref()).map(|r| r.leaves()).unwrap_or_default(),
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

    pub fn tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active.min(self.tabs.len().saturating_sub(1)))
    }

    pub fn tab_mut(&mut self) -> Option<&mut Tab> {
        let active = self.active.min(self.tabs.len().saturating_sub(1));
        self.tabs.get_mut(active)
    }

    pub fn closed(&self) -> bool {
        self.kept.as_ref().is_some_and(|s| s.closed)
    }

    pub fn status(&self) -> &'static str {
        if self.delete_pending {
            if self.close_error.is_some() { "Delete failed" } else { "Deleting" }
        } else if self.close_error.is_some() {
            if self.closed() { "Programs stopped · Save failed" } else { "Open · Close failed" }
        } else if self.closed() {
            "Closed"
        } else if self.kept.is_some() {
            "Closing"
        } else {
            "Open"
        }
    }

    pub fn counts(&self) -> (usize, usize) {
        self.kept.as_ref().map_or_else(|| (self.tabs.len(), self.panes().len()), |s| s.counts())
    }

    pub fn panes(&self) -> Vec<PaneId> {
        self.tabs.iter().flat_map(|t| t.panes()).collect()
    }
}

/// What a client's next frame is due for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirty {
    /// Nothing: no frame is due.
    No,
    /// What panes printed since its last frame, and nothing else that marks a client.
    Output,
    /// Anything.
    Yes,
}

/// The panes that printed since a client's last frame, in whatever workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::server) enum Printed {
    None,
    One(PaneId),
    Several,
}

impl Printed {
    fn add(&mut self, pane: PaneId) {
        *self = match *self {
            Printed::None => Printed::One(pane),
            Printed::One(p) if p == pane => Printed::One(p),
            Printed::One(_) | Printed::Several => Printed::Several,
        };
    }
}

pub struct Client {
    pub id: ClientId,
    pub out: clients::Outbox,
    pub attached: bool,
    pub cols: u16,
    pub rows: u16,
    pub workspace: u32,
    pub parser: InputParser,
    pub pending_since: Option<Instant>,
    pub mode: Mode,
    pub last_account_agent: Option<String>,
    /// The login pane opened by this client's Add account form.
    pub adding_account: Option<PaneId>,
    pub encoder: Encoder,
    /// The last frame composed for it, kept to compose the next one over it.
    pub(in crate::server) frame: ratatui::buffer::Buffer,
    pub(in crate::server) memo: render::FrameMemo,
    /// Whether it has the sidebar, once it showed or hid it itself: `[sidebar] visible` says
    /// until then.
    pub sidebar: Option<bool>,
    pub dirty: Dirty,
    pub(in crate::server) printed: Printed,
    pub last_input: Instant,
    pub terminal_focused: bool,
    pub drag: Option<ui::Drag>,
    pub toast: Option<(String, Instant)>,
    pub toast_action: Option<(PaneId, attention::ToastAction)>,
    /// A message to show once the one shown now goes.
    pub notice: Option<String>,
    /// Click bookkeeping for double/triple click selection.
    pub last_click: Option<(Instant, u16, u16, u8)>,
    /// The first session's welcome note is showing.
    pub welcome: bool,
    /// The client runs on another machine: `localhost` there is not the server's.
    pub remote: bool,
    /// Its terminal reports every pointer motion (mode 1003), for a pane that asked for it.
    pub pointer_motion: bool,
    /// Its terminal can send every key with its place and its text (kitty keyboard protocol):
    /// unknown until it answers.
    pub keys_full: Option<bool>,
    /// Its terminal sends every key with its place, for the leader menu.
    pub keys_by_place: bool,
    /// It asked to be told which pane it shows (a kraal attached here from a pane of another
    /// kraal): the pane it was last told, until it changes.
    pub told_shown: Option<Option<PaneId>>,
    /// A paste arriving in pieces.
    pub(in crate::server) paste: Option<input::PasteFlow>,
    /// The process that connected, where the system tells.
    pub peer: Option<u32>,
}

/// A line of the messages log.
pub struct Message {
    pub at: std::time::SystemTime,
    pub text: String,
    pub warning: bool,
}

/// Messages kept for the log.
const MAX_MESSAGES: usize = 200;

/// A control request waiting for a pane condition.
struct PendingWait {
    client: ClientId,
    out: clients::Outbox,
    pane: PaneId,
    until: Vec<String>,
    deadline: Option<Instant>,
}

pub struct Server {
    pub session: String,
    pub config: Config,
    pub keymap: Keymap,
    pub env: PaneEnv,
    pub tx: EventSender,
    pub panes: HashMap<PaneId, Pane>,
    pub workspaces: Vec<Workspace>,
    pub workspace_order: Vec<persist::WorkspaceKey>,
    pub clients: HashMap<ClientId, Client>,
    next_id: u32,
    /// The client whose terminal size drives pane sizes.
    pub primary: Option<ClientId>,
    /// The primary client's main area: the panes are sized for it, and each client draws them
    /// from where its own panes start (see [`Server::client_area`]).
    pub layout_area: Rect,
    waits: Vec<PendingWait>,
    persist_dirty: bool,
    last_persist: Instant,
    last_tick: Instant,
    /// Nothing happened for a while: the tick comes with the scan, or seldom (see
    /// [`Server::next_tick`]).
    idle: bool,
    /// The scan must come every [`SCAN_EVERY`] (see [`Server::scan_needed`]).
    scan_polled: bool,
    /// Kubeconfigs are in use: they are read at every scan.
    kube_polled: bool,
    /// When a client last said something (a key, a request, a connection).
    last_contact: Instant,
    /// When the contexts, the accounts' usage and the listening ports were last read.
    last_scan: Instant,
    next_render: Option<Instant>,
    /// When an image that stopped moving may be drawn as sixel where it stopped. Kept apart from
    /// `next_render`, which it would hold back: other changes are drawn meanwhile.
    image_wake: Option<Instant>,
    /// When drawing the last frame finished: composing it is not a quiet spell in the output.
    last_frame: Instant,
    /// Frames in a row that output was already waiting for (see [`Server::schedule_render`]).
    streak: u8,
    /// Panes just typed into, until when their output is drawn at once (it is the echo).
    echo: HashMap<PaneId, Instant>,
    /// Panes just resized, until when their output is drawn at once (it is their picture at the
    /// new size, see [`Server::relayout`]).
    redraws: HashMap<PaneId, Instant>,
    /// When what was last marked to draw has been quiet long enough to be drawn without waiting
    /// for the frame gap (see [`Server::schedule_render`]).
    settle: Option<Instant>,
    quitting: bool,
    /// `kraal kill` requests, answered once the last state is written (or not).
    kill_waiters: Vec<ClientId>,
    /// The server stops for `kraal restart`: attached clients wait for the next one.
    restarting: bool,
    /// Colours of the outer terminal, learnt from OSC 10/11 replies.
    pub outer_colors: Option<(String, String)>,
    /// What each client's terminal can show of the panes' images, and has been sent.
    images: HashMap<ClientId, graphics::ClientImages>,
    /// Images ready to draw as sixel.
    fitted: graphics::Fitted,
    /// The panes' pictures being prepared off the loop (see `prepare_images`).
    preparing: usize,
    /// Warnings not shown yet: they go to the clients and the messages log.
    pub warnings: Vec<String>,
    /// Why `accounts.toml` could not be read last time, so that it is said once, not every scan.
    pub accounts_error: Option<String>,
    /// What kraal told the clients, for the messages log.
    pub messages: std::collections::VecDeque<Message>,
    /// Path and name of the workspace definitions on disk that could be read, refreshed
    /// periodically.
    pub def_files: Vec<(PathBuf, String)>,
    last_def_scan: Instant,
    /// Servers from the inventory, workspaces and ~/.ssh/config.
    pub known_servers: Vec<crate::servers::Known>,
    /// ssh destinations already looked at for auto-adding.
    discovered: std::collections::HashSet<String>,
    pub accounts: crate::accounts::Accounts,
    existing_checks: HashMap<String, agents::ExistingCheck>,
    /// Name of the workspace whose definition is being started (for account rules).
    starting_workspace: Option<String>,
    pub usage: crate::accounts::UsageMap,
    /// The reset of a usage window that comes next (Unix seconds), and when to wake for it (see
    /// [`Server::await_usage_reset`]).
    usage_reset: Option<(i64, Instant)>,
    last_codex_poll: Instant,
    limit_alerts: HashMap<PaneId, agents::LimitAlert>,
    /// The limit message each agent pane shows, as already counted: a message staying on
    /// screen is one observation, not a new one at every check.
    screen_limits: HashMap<PaneId, (bool, f64)>,
    /// Account changes shared by every client attached to this session.
    switches: HashMap<PaneId, agents::AccountSwitch>,
    pane_edits: HashMap<PaneId, pane_ui::PaneEdit>,
    observed_agents: HashMap<PaneId, agents::ObservedAgent>,
    /// Panes whose account is over its threshold, already reported as an event.
    limit_events: std::collections::HashSet<PaneId>,
    /// Branch and changes per directory (None: not a repository).
    pub git: HashMap<PathBuf, Option<crate::git::GitInfo>>,
    /// The directories panes are in: when to ask git about them again, and the watches on
    /// them, on their repositories and on the configuration's directories.
    dirwatch: dirwatch::DirWatch,
    git_requests: Sender<PathBuf>,
    /// Starts the programs of the panes of a batch (see `crate::pane::launcher`).
    launches: Sender<crate::pane::Pending>,
    /// Current Kubernetes context per `KUBECONFIG` value in use.
    pub kube: HashMap<String, crate::context::KubeFile>,
    kube_cache: crate::context::KubeCache,
    /// Terraform workspaces of the directories panes are in, and of those Terraform runs worked
    /// on (`-chdir`, `TF_DATA_DIR`).
    tf_dirs: contexts::TfDirs,
    /// The `[hints.patterns]` of the config, compiled.
    pub hint_patterns: crate::hints::Custom,
    /// `kraal events` clients.
    subscribers: Vec<events::Subscriber>,
    events: events::EventLog,
    /// Panes `pane.opened` was said for.
    announced: std::collections::HashSet<PaneId>,
    hooks: events::Hooks,
    /// SSH server and production reason of each pane at the last tick, to notice changes.
    pane_states: events::PaneStates,
    /// Runbooks being prepared, by client.
    drafts: HashMap<ClientId, runbooks::Draft>,
    /// Plans shown by `kraal runbook run`, waiting for the yes that runs them.
    plans: HashMap<String, (Instant, runbooks::Draft)>,
    pub runs: Vec<runbooks::Run>,
    /// Runs written in the ledger whose targets wait for that to be done to start.
    launching: Vec<crate::runbooks::LedgerRun>,
    /// Process groups of timed-out targets whose panes were closed before they had ended.
    stray_groups: Vec<runbooks::StrayGroup>,
    /// What the panes of runs print, streamed to their logs.
    pub run_output: HashMap<PaneId, crate::runbooks::OutputLog>,
    /// Markdown runbooks in progress, by client.
    md_sessions: HashMap<ClientId, runbooks::MdSession>,
    /// What to do with the result of each background job.
    jobs: HashMap<u32, JobDone>,
    /// Files of servers being edited in the local editor.
    edits: Vec<edits::Edit>,
    /// Links to open on a client once their tunnel is up: (client, tunnel id, URL).
    pending_opens: Vec<(ClientId, u32, String)>,
    /// Preview panes being read again.
    preview_loads: std::collections::HashSet<PaneId>,
    /// The thread that writes the session state.
    persister: Sender<persistence::Persist>,
    /// What that thread and the event loop share beside the jobs.
    persisting: Arc<Mutex<persistence::Persisting>>,
    /// The state the last save sent wrote (compact JSON).
    persisted: Option<Vec<u8>>,
    /// What is saved of each pane's scrollback.
    pub scrollback: HashMap<PaneId, persist::SavedScrollback>,
    /// The saved workspaces that could not be restored: saved again as they were.
    pub unrestored: Vec<persist::WorkspaceSnap>,
    pub deleted_workspaces: Vec<persist::DeletedWorkspace>,
    pub removed_sources: Vec<PathBuf>,
    /// Starts the names of the scrollback files this server makes, so that none is a file an
    /// earlier server's state names.
    scrollback_tag: String,
    /// Writes run logs and the run ledger, in order, off the event loop.
    run_logs: Sender<crate::runbooks::LogJob>,
    pub tunnels: Vec<net::Tunnel>,
    /// Servers whose kraal session's agents are listed.
    pub remotes: Vec<remote::Watch>,
    /// Ports mentioned in pane output, and those of them something listens on.
    port_candidates: HashMap<u16, PaneId>,
    pub ports: std::collections::BTreeMap<u16, PaneId>,
    /// Panes with output waiting, in round-robin order.
    ready: std::collections::VecDeque<PaneId>,
    ready_set: std::collections::HashSet<PaneId>,
    /// Room for the output being parsed, reused from one read to the next.
    output_buf: Vec<u8>,
    /// Output taken from panes and not parsed yet: parsing stopped for something that came
    /// (a key), and goes on before any more of their output (see `pump_output`).
    unparsed: HashMap<PaneId, Vec<u8>>,
    /// Panes printing without a pause that had no output waiting: looked at again at
    /// `watch_at`, without their readers waking the server meanwhile.
    watched: Vec<PaneId>,
    watch_at: Option<Instant>,
    /// Panes whose process exited, with its pid and code: their last output is read first, for
    /// a bounded time (something left in the background may keep writing).
    exiting: HashMap<PaneId, (Option<u32>, Option<i32>, Instant)>,
    /// Input going to panes whose context is being read again, kept until the read ends, in the
    /// order it was typed, with where it goes (see `Server::hold_input`).
    held_input: Vec<input::Held>,
    /// Panes told they have the focus (mode 1004).
    focus_reported: std::collections::HashSet<PaneId>,
    /// The other panes' turn at output was skipped for a frame due (see `pump_output`).
    turn_skipped: bool,
    /// What a client typed last only went to programs (see `client_events`).
    typed_quietly: bool,
    /// Counts what may have changed what the chrome (bars, sidebar, frames) shows, besides the
    /// panes' output: everything but output read and keys that only went to programs.
    chrome_gen: u64,
    /// What the chrome shows of each pane, while it holds (see `Server::pane_facts`).
    facts: render::FactsCache,
    /// The panes that printed since the last frames: what the chrome shows of them is looked at
    /// before the next ones (see `Server::show_what_output_changed`).
    printed: Vec<PaneId>,
    /// The last layouts worked out, most recent last (see `Server::tiled_rects`).
    tilings: std::cell::RefCell<Vec<geometry::Tiling>>,
    /// Whether the bottom bar shows: as `[bar] keys` says, until toggled for the session.
    pub bottom_bar: bool,
    /// Whether a client was told how to stop the broadcast its key started.
    pub broadcast_tip_shown: bool,
    /// What was copied, to paste again, and named text (`kraal buffer`).
    pub buffers: crate::buffers::Buffers,
    /// The buffers changed since they were last written (when they are kept across restarts).
    buffers_dirty: bool,
    /// Panes closed since the last tick: the memory they held goes back to the system with it.
    panes_closed: bool,
}

/// Finishes a background job on the server thread with its result.
type JobDone = Box<dyn FnOnce(&mut Server, Box<dyn std::any::Any + Send>)>;

/// The periodic work (agents, previews, runbooks...) runs this often while something happens:
/// output, keys, requests, or work that needs watching (see [`Server::busy`]).
const TICK: Duration = Duration::from_millis(400);
/// Once nothing has for this long, it runs only with the scan...
const ACTIVE_FOR: Duration = Duration::from_secs(3);
/// ...or this often when nothing needs one (see [`Server::scan_needed`]): what changes meanwhile
/// is told (see [`dirwatch`]), and the first key or request after a quiet spell runs the tick
/// before anything else.
const IDLE_EVERY: Duration = Duration::from_secs(30);
/// The contexts, the accounts' usage and the ports something listens on are read again this
/// often when they have to be looked at.
const SCAN_EVERY: Duration = Duration::from_secs(3);
const FRAME: Duration = Duration::from_millis(16);
const INPUT_FLUSH: Duration = Duration::from_millis(30);
/// A pane whose last output is younger than this is printing without a pause...
const STREAM_PAUSE: Duration = Duration::from_millis(2);
/// ...and its output is looked for this often (frames are drawn no sooner).
const WATCH_STREAMS: Duration = Duration::from_millis(1);
/// The least time between frames for output: output after a quiet spell is drawn at once, more
/// of it that soon waits so that a burst makes one frame.
const FRAME_GAP: Duration = Duration::from_millis(2);
/// The same while output keeps coming: frame after frame found it waiting ([`BUSY_AFTER`]).
const BUSY_FRAME_GAP: Duration = Duration::from_millis(8);
const BUSY_AFTER: u8 = 3;
/// How long after a key the output of the pane it went to is drawn at once, as its echo.
const ECHO_WAIT: Duration = Duration::from_millis(250);
/// How long after a pane is resized its output is drawn at once, as its picture at the new size:
/// a shell answers within a millisecond, a program on a server after a round trip.
const REDRAW_WAIT: Duration = Duration::from_millis(250);
/// Output is parsed this much at a time, so that what comes meanwhile (a key, its echo) does not
/// wait for all that a program printed to be parsed...
const PARSE_SLICE: usize = 8 * 1024;
/// ...and this much while someone types (see `TYPING`): a key and its echo each wait for a
/// slice at most.
const TYPING_SLICE: usize = 1024;
/// How long after a key someone counts as typing.
const TYPING: Duration = Duration::from_secs(1);

/// Whether what was last done at `last` is due again at `now`, done `every` so often: a little
/// early rather than a whole tick late.
fn due(now: Instant, last: Instant, every: Duration) -> bool {
    now.saturating_duration_since(last) + Duration::from_millis(50) >= every
}

/// What a panic said, when it said it in text.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    let text = payload.downcast_ref::<&str>().copied();
    text.or_else(|| payload.downcast_ref::<String>().map(String::as_str)).unwrap_or("it panicked")
}

/// Why a server did not start: another kraal has its session. A server ends with
/// [`HELD_STATUS`] then, and the kraal that started it waits for that one's socket, or says who
/// has the session.
#[derive(Debug)]
pub struct Held(String);

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Held {}

/// What `kraal server` exits with on [`Held`] (a temporary failure, as sysexits names 75).
pub const HELD_STATUS: i32 = 75;

/// Run a session server in the foreground until it is told to exit.
pub fn run(session: &str) -> Result<()> {
    let mut ready = take_ready_pipe();
    // Kept from `kraal forget` until this server has the session, when a restart started it.
    let restarting = paths::Restarting::inherited(session);
    // What this process inherited stays out of the panes' programs.
    crate::pane::close_inherited();
    crate::pane::reserve_descriptors();
    let socket = paths::socket_path(session);
    paths::private_runtime_dir().context("checking the runtime directory")?;
    // Held until this server is gone, and what writes for it with it ([`spawn_persister`]): the
    // socket, and the session's files, are its alone.
    #[cfg(debug_assertions)]
    held_back_for_tests();
    let Some(held) = paths::SessionLock::acquire(session).context("locking the session")? else {
        return Err(Held(format!("another kraal holds session {session:?}: its server, or kraal forget")).into());
    };
    let held = Arc::new(held);
    if let Some(restarting) = restarting {
        restarting.end();
    }
    // A server kraal 0.2.0 started holds no lock.
    let Some(listener) = paths::listen_alone(&socket).with_context(|| format!("binding {}", socket.display()))? else {
        return Err(Held(format!("session {session:?} is already running")).into());
    };
    let own_socket = paths::file_id(&socket);
    let (config, mut warnings) =
        Config::load_with_warnings().unwrap_or_else(|e| (Config::default(), vec![format!("config: {e:#}")]));
    // In the log too: the list is shown to who attaches, and none may.
    if let Some(refused) = paths::runtime_dir_refused() {
        eprintln!("kraal: warning: {refused}");
        warnings.push(refused);
    }
    let (mut keymap, kw) = Keymap::build(&config.keys);
    warnings.extend(kw);
    warnings.extend(keymap.bind_tools(&config.tools));
    warnings.extend(crate::tools::check(&config.tools));
    let (hint_patterns, hw) = crate::hints::Custom::build(&config.hints, &config.tools);
    warnings.extend(hw);
    let (hook_list, hkw) = events::read_hooks(&config.hooks);
    warnings.extend(hkw);
    let (theme, tw) = crate::theme::Theme::from_config(&config.theme);
    crate::theme::set(theme);
    warnings.extend(tw);
    let bottom_bar = config.bar.keys != crate::config::BarKeys::Off;

    let (tx, rx) = queue::channel(4096);
    let persist_tx = tx.clone();
    let persisting = Arc::new(Mutex::new(persistence::Persisting::default()));
    let logs_tx = tx.clone();
    {
        let tx = tx.clone();
        crate::threads::named("accept").spawn(move || {
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
        crate::threads::named("signals").spawn(move || {
            if signals.forever().next().is_some() {
                let _ = tx.send(Event::Shutdown);
            }
        })?;
    }

    let git_tx = tx.clone();
    let watch_tx = tx.clone();
    // Where nothing can be watched, git is asked about repositories every few seconds instead.
    let watcher = crate::watch::Watcher::new(move |changes| {
        let _ = watch_tx.send(Event::Watched(changes));
    })
    .ok();
    let env = PaneEnv {
        socket: socket.clone(),
        session: session.to_string(),
        shell: config.shell(),
        scrollback: config.scrollback,
        graphics: std::sync::Arc::new(crate::graphics::Shared::new(
            config.graphics.enabled,
            config.graphics.memory_mb.saturating_mul(1 << 20),
        )),
    };
    let buffers = crate::buffers::Buffers::new(config.buffers.limit, config.buffers.max_bytes());
    let mut server = Server {
        session: session.to_string(),
        config,
        keymap,
        env,
        tx,
        panes: HashMap::new(),
        workspaces: Vec::new(),
        workspace_order: Vec::new(),
        clients: HashMap::new(),
        next_id: 1,
        primary: None,
        // A 120×40 terminal's, until a client attaches.
        layout_area: Rect::new(0, 1, 120, 39 - u16::from(bottom_bar)),
        waits: Vec::new(),
        persist_dirty: false,
        last_persist: Instant::now(),
        last_tick: Instant::now(),
        idle: false,
        scan_polled: true,
        kube_polled: false,
        last_contact: Instant::now(),
        last_scan: Instant::now() - SCAN_EVERY,
        next_render: None,
        image_wake: None,
        last_frame: Instant::now(),
        streak: 0,
        echo: HashMap::new(),
        redraws: HashMap::new(),
        settle: None,
        quitting: false,
        kill_waiters: Vec::new(),
        restarting: false,
        outer_colors: None,
        images: HashMap::new(),
        fitted: Default::default(),
        preparing: 0,
        warnings,
        accounts_error: None,
        messages: Default::default(),
        def_files: Vec::new(),
        last_def_scan: Instant::now(),
        known_servers: Vec::new(),
        discovered: Default::default(),
        accounts: crate::accounts::Accounts::empty(),
        existing_checks: HashMap::new(),
        starting_workspace: None,
        usage: Default::default(),
        usage_reset: None,
        last_codex_poll: Instant::now() - Duration::from_secs(3600),
        limit_alerts: HashMap::new(),
        screen_limits: HashMap::new(),
        switches: HashMap::new(),
        pane_edits: HashMap::new(),
        observed_agents: HashMap::new(),
        limit_events: Default::default(),
        ready: Default::default(),
        ready_set: Default::default(),
        output_buf: Vec::new(),
        unparsed: HashMap::new(),
        watched: Vec::new(),
        watch_at: None,
        exiting: Default::default(),
        focus_reported: Default::default(),
        held_input: Vec::new(),
        bottom_bar,
        turn_skipped: false,
        typed_quietly: false,
        chrome_gen: 0,
        facts: Default::default(),
        printed: Vec::new(),
        tilings: Default::default(),
        broadcast_tip_shown: false,
        buffers,
        buffers_dirty: false,
        panes_closed: false,
        tunnels: Vec::new(),
        remotes: Vec::new(),
        port_candidates: HashMap::new(),
        ports: Default::default(),
        git: HashMap::new(),
        dirwatch: dirwatch::DirWatch::new(watcher),
        git_requests: worktrees::spawn_git_worker(git_tx)?,
        launches: crate::pane::launcher(),
        kube: HashMap::new(),
        kube_cache: Default::default(),
        tf_dirs: Default::default(),
        hint_patterns,
        subscribers: Vec::new(),
        events: Default::default(),
        announced: Default::default(),
        hooks: events::Hooks::new(hook_list),
        pane_states: Default::default(),
        drafts: HashMap::new(),
        plans: HashMap::new(),
        runs: Vec::new(),
        launching: Vec::new(),
        stray_groups: Vec::new(),
        run_output: HashMap::new(),
        md_sessions: HashMap::new(),
        jobs: HashMap::new(),
        edits: Vec::new(),
        preview_loads: Default::default(),
        pending_opens: Vec::new(),
        persister: spawn_persister(persist_tx, persisting.clone(), held.clone())?,
        persisting,
        persisted: None,
        scrollback: HashMap::new(),
        unrestored: Vec::new(),
        deleted_workspaces: Vec::new(),
        removed_sources: Vec::new(),
        scrollback_tag: format!("{}-{}", crate::accounts::now(), std::process::id()),
        run_logs: {
            let warn = logs_tx;
            crate::runbooks::spawn_log_writer(move |w| {
                let _ = warn.send(Event::Warning(w));
            })?
        },
    };
    server.scan_definitions();
    server.load_buffers();
    let result = server.restore().and_then(|()| {
        // On failure, keep the pipe open until cleanup removes the socket: the caller looks
        // for a listening server once the pipe closes, and this one cannot serve it.
        if let Some(mut ready) = ready.take() {
            use std::io::Write;
            let _ = ready.write_all(b"1");
        }
        let result = server.event_loop(&rx);
        server.shutdown();
        result
    });
    // Its own socket, as far as a look before the removal tells: not the one of a kraal 0.2.0
    // (it takes no lock) that listens there since a cleaner of /tmp removed this one. No other
    // file has this one's number while this server listens: it holds the file until it is gone.
    if own_socket.is_some() && paths::file_id(&socket) == own_socket {
        let _ = std::fs::remove_file(&socket);
    }
    // Nothing asks the writer for more: it lets go of the session as it ends, and the session is
    // let go here once it has. A writer stuck in a write keeps it until this process is gone,
    // and that write with it.
    drop(server);
    let limit = Instant::now() + Duration::from_secs(2);
    while Arc::strong_count(&held) > 1 && Instant::now() < limit {
        std::thread::sleep(Duration::from_millis(5));
    }
    result
}

/// Tests (debug builds only) hold a server back before it takes its session, for as long as the
/// file `KRAAL_TEST_HOLD_SERVER` names is there: the moment in which a restart has left the
/// session to the server it started.
#[cfg(debug_assertions)]
fn held_back_for_tests() {
    let Some(marker) = std::env::var_os("KRAAL_TEST_HOLD_SERVER").map(std::path::PathBuf::from) else { return };
    while marker.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Tests (debug builds only) read how each frame was composed from the file `KRAAL_TEST_FRAMES`
/// names, a line a frame: the client, then `output` for a frame made from what one pane printed
/// alone (see `Server::compose_output`), `whole` for any other.
#[cfg(debug_assertions)]
fn note_frame(client: ClientId, from_output: bool) {
    use std::io::Write;
    let Some(path) = std::env::var_os("KRAAL_TEST_FRAMES") else { return };
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(log, "{client} {}", if from_output { "output" } else { "whole" });
    }
}

/// The pipe the client that started this server waits on (see `client::ensure_server`), taken
/// out of the environment so that nothing this server starts sees it. Only the write end of a
/// pipe is taken: a stale or odd value names nothing to write to.
fn take_ready_pipe() -> Option<std::fs::File> {
    use std::os::fd::FromRawFd;
    let value = std::env::var_os(crate::client::READY_FD_VAR)?;
    // SAFETY: the server has not started any thread yet.
    unsafe { std::env::remove_var(crate::client::READY_FD_VAR) };
    let fd: libc::c_int = value.to_str()?.parse().ok().filter(|fd| *fd > 2)?;
    // SAFETY: fstat and fcntl only look at the descriptor, and fail on one that is not open.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 || st.st_mode & libc::S_IFMT != libc::S_IFIFO {
            return None;
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || flags & libc::O_ACCMODE != libc::O_WRONLY {
            return None;
        }
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        // SAFETY: the client gave this descriptor to this process alone.
        Some(std::fs::File::from_raw_fd(fd))
    }
}

impl Server {
    /// Run `work` on a helper thread (slow git, commands), then `then` with its result on the
    /// server thread, so nothing slow ever blocks the event loop. `false` when no thread could
    /// be started: `then` never runs, nor does it when `work` panics (a warning says so).
    pub fn background<T: Send + 'static>(
        &mut self,
        work: impl FnOnce() -> T + Send + 'static,
        then: impl FnOnce(&mut Server, T) + 'static,
    ) -> bool {
        let id = self.alloc_id();
        self.jobs.insert(
            id,
            Box::new(move |server, result| {
                if let Ok(value) = result.downcast::<T>() {
                    then(server, *value);
                }
            }),
        );
        let tx = self.tx.clone();
        let spawned = crate::threads::named("job").spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
            let _ = tx.send(Event::Job(id, result.map(|value| Box::new(value) as Box<dyn std::any::Any + Send>)));
        });
        if let Err(e) = spawned {
            self.jobs.remove(&id);
            self.warnings.push(format!("cannot start a background job: {e}"));
            return false;
        }
        true
    }

    /// Show `mode` (a question a background job prepared) unless the client is busy with
    /// another picker or prompt: then the question is dropped (the client is told to ask again).
    pub fn set_mode_if_idle(&mut self, client: ClientId, mode: Mode) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        if !matches!(c.mode, Mode::Normal | Mode::Leader { .. }) {
            c.toast =
                Some(("finish what you are doing, then ask again".into(), Instant::now() + Duration::from_secs(3)));
            c.toast_action = None;
            return;
        }
        c.mode = mode;
        c.dirty = Dirty::Yes;
        self.mark_dirty_client(client);
    }

    pub fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn event_loop(&mut self, rx: &EventReceiver) -> Result<()> {
        rx.assert_sender(&self.tx);
        loop {
            let timeout = if self.ready.is_empty() {
                self.next_deadline().saturating_duration_since(Instant::now())
            } else {
                Duration::ZERO
            };
            if !self.event_phase(rx, rx.wait(timeout)) {
                break;
            }
            // …then a slice of program output, interrupted as soon as something else arrives.
            self.pump_output(Duration::from_millis(6), rx);
            self.timers();
            // Timers close panes too.
            self.follow_focus();
            if self.quitting {
                break;
            }
            if self.image_wake.is_some_and(|t| t <= Instant::now()) {
                self.image_wake = None;
                self.next_render = Some(Instant::now());
            }
            // What changed last has been quiet for a while: its frame waits no longer.
            if let Some(at) = self.settle.filter(|t| *t <= Instant::now()) {
                self.settle = None;
                if let Some(r) = self.next_render {
                    self.next_render = Some(r.min((self.last_frame + FRAME_GAP).max(at)));
                }
            }
            if self.next_render.is_some_and(|t| t <= Instant::now()) {
                self.render_clients();
            }
        }
        Ok(())
    }

    fn event_phase(&mut self, rx: &EventReceiver, first: Wait) -> bool {
        let selected = matches!(first, Wait::Overflow);
        match first {
            Wait::Event(ev) => self.handle_and_follow(ev),
            Wait::Disconnected => return false,
            Wait::Overflow | Wait::Timeout => {}
        }
        // Keep ordinary events in order, including the one selection took. A selected token
        // remains an obligation until these handlers finish.
        while let Ok(ev) = rx.try_recv() {
            self.handle_and_follow(ev);
        }
        let notifications = rx.reconcile(selected);
        if notifications.panes {
            self.wake();
            self.reconcile_panes();
            self.finish_exits();
        }
        if notifications.wake {
            self.handle_and_follow(Event::Wake);
        }
        true
    }

    fn reconcile_panes(&mut self) {
        for pane in self.panes.values() {
            if pane.has_output() && self.ready_set.insert(pane.id) {
                self.ready.push_back(pane.id);
            }
        }
    }

    /// Parse waiting pane output for up to `budget`: the panes clients are looking at first,
    /// a chunk each whatever else waits, then more for at most half of it, then a chunk of each
    /// pane just resized (see [`Server::redrawn`]), then the others in turn, one chunk each.
    /// Stops early when a control event arrives or a frame is due, but the others always get a
    /// chunk: a pane printing without a pause keeps the queue busy, and must not starve the
    /// rest. A frame due once the focused panes are read (the echo of a key) goes before the
    /// others' output, every other time at most. A chunk is parsed a slice at a time: one that
    /// comes while it is parsed (a key) waits for a slice, not for all of it.
    fn pump_output(&mut self, budget: Duration, rx: &EventReceiver) {
        let start = Instant::now();
        let focused: Vec<PaneId> =
            self.clients.values().filter(|c| c.attached).filter_map(|c| self.client_focus(c.id)).collect();
        let mut data = std::mem::take(&mut self.output_buf);
        // A frame is due at `next_render`, or when an image that stopped moving may be drawn.
        let more = |s: &Server, budget: Duration| {
            let now = Instant::now();
            start.elapsed() < budget
                && rx.is_empty()
                && [s.next_render, s.image_wake].into_iter().flatten().all(|t| t > now)
        };
        for p in focused {
            let mut first = true;
            while std::mem::take(&mut first) || more(self, budget / 2) {
                if !self.take_output(p, &mut data, true) {
                    break;
                }
                if !self.parse_output(p, &data, rx, false) {
                    break;
                }
            }
        }
        // The frame due after a resize shows the panes as they drew themselves again, not as
        // they were left: what they printed since is read before it.
        for p in self.redrawn() {
            if self.take_output(p, &mut data, true) {
                self.parse_output(p, &data, rx, false);
            }
        }
        // A frame due (an echo) goes before the others' turn, but not twice in a row: a pane
        // printing in the focus, with frames always due, must not starve the others. Their turn
        // is then a slice: the frame waits no longer than that.
        let now = Instant::now();
        let due = [self.next_render, self.image_wake].into_iter().flatten().any(|t| t <= now);
        let skip = due && !self.turn_skipped;
        self.turn_skipped = skip;
        let mut served = skip;
        while !std::mem::replace(&mut served, true) || more(self, budget) {
            let Some(id) = self.ready.pop_front() else { break };
            if self.take_output(id, &mut data, true) {
                self.parse_output(id, &data, rx, due);
                self.ready.push_back(id);
            } else if self.streaming(id) {
                self.watched.push(id);
                self.watch_at.get_or_insert(Instant::now() + WATCH_STREAMS);
            } else {
                self.ready_set.remove(&id);
            }
        }
        self.output_buf = data;
    }

    /// The panes resized a moment ago (see `redraws`) that have output waiting, in the order it
    /// came.
    fn redrawn(&self) -> Vec<PaneId> {
        if self.redraws.is_empty() {
            return Vec::new();
        }
        let now = Instant::now();
        let awaited = |p: &PaneId| self.redraws.get(p).is_some_and(|until| now <= *until);
        self.ready.iter().copied().filter(awaited).collect()
    }

    /// Parse output `data` of pane `id`, a slice at a time, until it is all parsed (true) or
    /// something waits in `rx` (after one slice with `one`): then the rest is parsed before any
    /// more of the pane's output, and the pane stays in the queue for it.
    fn parse_output(&mut self, id: PaneId, data: &[u8], rx: &EventReceiver, one: bool) -> bool {
        let typing = self.clients.values().any(|c| c.attached && c.last_input.elapsed() < TYPING);
        let slice = if typing { TYPING_SLICE } else { PARSE_SLICE };
        let mut at = 0;
        while at < data.len() {
            let end = (at + slice).min(data.len());
            self.pane_output(id, &data[at..end]);
            at = end;
            if at < data.len() && (one || !rx.is_empty()) && self.panes.contains_key(&id) {
                self.unparsed.insert(id, data[at..].to_vec());
                if self.ready_set.insert(id) {
                    self.ready.push_back(id);
                }
                return false;
            }
        }
        true
    }

    /// Whether pane `id` printed just now: the server then comes back for its output on its own
    /// (every [`WATCH_STREAMS`]) instead of being woken for each read of it.
    fn streaming(&self, id: PaneId) -> bool {
        self.panes.get(&id).is_some_and(|p| p.last_output.elapsed() < STREAM_PAUSE)
    }

    /// Take pane `id`'s waiting output into `data`, what was left unparsed first; false when
    /// there was none. With `watch`, a pane printing without a pause is looked at again on its
    /// own (see `streaming`).
    fn take_output(&mut self, id: PaneId, data: &mut Vec<u8>, watch: bool) -> bool {
        if let Some(rest) = self.unparsed.remove(&id) {
            *data = rest;
            return true;
        }
        let comes_back = watch && self.streaming(id);
        self.panes.get(&id).is_some_and(|p| p.take_output_into(data, comes_back))
    }

    fn next_deadline(&self) -> Instant {
        let mut d = self.next_tick();
        for r in [self.next_render, self.image_wake].into_iter().flatten() {
            d = d.min(r);
        }
        if let Some(s) = self.settle.filter(|_| self.next_render.is_some()) {
            d = d.min(s);
        }
        if let Some(g) = self.dirwatch.next() {
            d = d.min(g);
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
        for (_, _, until) in self.exiting.values() {
            d = d.min(*until);
        }
        if let Some(w) = self.watch_at {
            d = d.min(w);
        }
        if let Some((_, wake)) = self.usage_reset {
            d = d.min(wake);
        }
        d.max(Instant::now() + Duration::from_millis(1))
    }

    fn timers(&mut self) {
        let now = Instant::now();
        if self.watch_at.is_some_and(|t| t <= now) {
            self.watch_at = None;
            self.ready.extend(self.watched.drain(..));
        }
        if self.usage_reset.is_some_and(|(_, wake)| wake <= now) {
            self.await_usage_reset();
        }
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
            let notice = self.clients.get_mut(&id).and_then(|c| {
                c.toast = None;
                c.toast_action = None;
                c.notice.take()
            });
            if let Some(text) = notice {
                self.toast(id, text);
            }
            self.mark_dirty_client(id);
        }
        // Synchronized updates that timed out: what they held is drawn, and what the emulator
        // has to answer or report goes out.
        let flushed: Vec<(PaneId, Vec<PaneSignal>)> =
            self.panes.iter_mut().filter_map(|(id, p)| p.poll_sync_timeout().map(|s| (*id, s))).collect();
        for (id, signals) in flushed {
            self.pane_signals(id, signals);
            self.mark_dirty_pane(id);
        }
        if !self.exiting.is_empty() || !self.held_input.is_empty() {
            self.chrome_changed();
        }
        self.finish_exits();
        self.release_held();
        self.git_due(now);
        if now >= self.next_tick() {
            self.last_tick = now;
            // What it looks at (agents, processes, contexts, usage) and the time it shows.
            self.chrome_changed();
            self.facts.keep(|id| self.panes.contains_key(&id));
            self.unparsed.retain(|id, _| self.panes.contains_key(id));
            self.tick(now);
            self.idle = self.quiet(now);
            self.scan_polled = self.scan_needed();
        }
        self.check_waits();
    }

    /// When the next tick comes: [`TICK`] after the last while something happens, else when
    /// the scan is due, else [`IDLE_EVERY`] after the last.
    fn next_tick(&self) -> Instant {
        match (self.idle, self.scan_polled) {
            (false, _) => self.last_tick + TICK,
            (true, true) => self.last_scan + SCAN_EVERY,
            (true, false) => self.last_tick + IDLE_EVERY,
        }
    }

    /// Whether what the scan reads has to be looked at every [`SCAN_EVERY`]: contexts in use or
    /// that cannot be watched, ports mentioned, repositories that cannot be watched, or the
    /// directories of the configuration and of the accounts' usage.
    fn scan_needed(&self) -> bool {
        self.contexts_polled()
            || self.dirwatch.polled()
            || !self.dirwatch.config_watched()
            || !self.port_candidates.is_empty()
            || !self.ports.is_empty()
    }

    /// Something happens: the periodic work comes at its usual pace again (at once when it was
    /// waiting longer than that).
    fn wake(&mut self) {
        self.idle = false;
    }

    /// Whether nothing happened for [`ACTIVE_FOR`] (no output, no key, no request) and nothing
    /// needs watching: the periodic work can wait.
    fn quiet(&self, now: Instant) -> bool {
        let recent = |t: Instant| now.saturating_duration_since(t) < ACTIVE_FOR;
        !recent(self.last_contact)
            && !self.panes.values().any(|p| recent(p.last_output))
            && !self.clients.values().any(|c| recent(c.last_input))
            && !self.busy()
    }

    /// Work that is looked at every tick and cannot wait: running runbooks and hooks, edits,
    /// tunnels and remote sessions, previews being read, panes about to close, conditions waited
    /// for, held input, and agents whose state is guessed from their screen while they work.
    fn busy(&self) -> bool {
        !self.runs.is_empty()
            || !self.launching.is_empty()
            || !self.stray_groups.is_empty()
            || !self.edits.is_empty()
            || !self.tunnels.is_empty()
            || !self.remotes.is_empty()
            || !self.pending_opens.is_empty()
            || !self.preview_loads.is_empty()
            || !self.held_input.is_empty()
            || !self.waits.is_empty()
            || !self.exiting.is_empty()
            || !self.pane_edits.is_empty()
            || self.switches.values().any(|s| {
                matches!(
                    s.stage,
                    agents::SwitchStage::Stopping
                        | agents::SwitchStage::Starting
                        | agents::SwitchStage::Restoring
                        | agents::SwitchStage::Recovering
                        | agents::SwitchStage::RecoveryStopping
                )
            })
            || self.hooks.busy()
            || self.panes.values().any(|p| {
                p.close_at.is_some()
                    || (p.agent.kind.is_some()
                        && !p.agent.hooked
                        && matches!(
                            p.agent.state,
                            crate::agent::AgentState::Working | crate::agent::AgentState::Blocked
                        ))
            })
    }

    /// Periodic work: agent detection, preview reloads, persistence.
    fn tick(&mut self, now: Instant) {
        // Contexts first: their reads run on threads of their own meanwhile, and are mostly
        // done when the tick ends (their answers then wake nothing).
        let scan = due(now, self.last_scan, SCAN_EVERY);
        if scan {
            self.last_scan = now;
            self.refresh_kube();
            self.refresh_tf_dirs();
            self.reload_usage();
        }
        let mut changed = Vec::new();
        let mut ssh_seen = Vec::new();
        let mut ssh_ended = Vec::new();
        let mut read_contexts = false;
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        // Freed, the memory of panes closed stays with the allocator, the server as large as it
        // was at its busiest, until it is given back.
        let mut freed = std::mem::take(&mut self.panes_closed);
        for id in ids {
            let Some(pane) = self.panes.get_mut(&id) else { continue };
            if pane.last_output.elapsed() > Duration::from_secs(1) {
                freed |= pane.rest();
            }
            if let crate::pane::Content::Preview(_) = &pane.content {
                continue;
            }
            if pane.exit_code.is_some() || pane.spec.login.is_some() {
                continue;
            }
            if let crate::pane::Content::Term(t) = &mut pane.content
                && std::mem::take(&mut t.input_dropped)
            {
                let name = pane.name.clone().unwrap_or_else(|| pane.title.clone());
                self.warnings.push(format!(
                    "{name} is not reading its input: what was typed or pasted beyond 16 MiB was dropped"
                ));
            }
            let before = pane.agent.state;
            let argv = pane.foreground_argv();
            if let Some(a) = &argv
                && ssh::runs_ssh(a)
            {
                ssh_seen.push((id, a[1..].to_vec()));
            } else if argv.is_some() && pane.ssh.is_some() {
                ssh_ended.push(id);
            }
            if let Some(a) = &argv
                && pane.ssh.is_none()
            {
                read_contexts |= contexts::observe_tools(&self.config.contexts, pane, a);
            }
            let agent = argv.as_deref().and_then(detect_agent);
            let mut moved = pane.agent.set_process(agent, crate::process::still_running);
            if pane.agent.kind.is_some() {
                let screen = pane.live_text();
                let quiet = pane.last_output.elapsed();
                moved |= pane.agent.observe(&screen, quiet);
            }
            if moved {
                changed.push((id, Some(before)));
            }
        }
        if freed {
            crate::pane::release_memory();
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
        // A context file just seen in use is read now, not at the next scan.
        if read_contexts {
            self.refresh_kube();
            self.refresh_tf_dirs();
        }
        self.check_previews();
        self.forget_ended_ssh(ssh_ended);
        self.save_buffers(false);
        self.tick_edits();
        if scan {
            self.refresh_observed_accounts();
        }
        self.check_switches();
        self.check_pane_edits();
        self.forget_ended_reports();
        self.diff_pane_states();
        self.hooks.reap();
        self.check_run_timeouts();
        self.show_warnings();
        self.echo.retain(|_, until| *until > now);
        self.redraws.retain(|_, until| *until > now);
        self.reconcile_panes();
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
        if scan {
            self.check_limits();
            self.refresh_ports();
        }
        self.refresh_git(now);
        self.check_tunnels();
        self.check_remotes();
        if self.last_codex_poll.elapsed() > Duration::from_secs(300) {
            self.poll_codex_usage();
        }
        let expired: Vec<PaneId> =
            self.panes.values().filter(|p| p.close_at.is_some_and(|t| t <= Instant::now())).map(|p| p.id).collect();
        for id in expired {
            self.close_pane(id);
        }
        self.save_scrollback(false);
        if self.persist_dirty || due(now, self.last_persist, Duration::from_secs(30)) {
            self.persist_unless_writing();
        }
        if due(now, self.last_def_scan, Duration::from_secs(5)) {
            self.scan_definitions();
        }
    }

    /// Handle `ev`, then tell the panes whose focus it changed. A pane's wake-up changes nothing
    /// yet: its output is read, and the focus followed, later in the loop.
    fn handle_and_follow(&mut self, ev: Event) {
        let wake_up = matches!(ev, Event::PaneReady(_));
        self.typed_quietly = false;
        self.handle(ev);
        // Output, and keys that only went to programs, move no focus.
        if !wake_up && !std::mem::take(&mut self.typed_quietly) {
            self.follow_focus();
        }
    }

    /// Read again, off the event loop (images take a while to decode), the previews whose file
    /// changed since it was read, or that are to be read (just opened, `r`).
    fn check_previews(&mut self) {
        let reloads: Vec<(PaneId, PathBuf)> = self
            .panes
            .values()
            .filter_map(|p| match &p.content {
                crate::pane::Content::Preview(v) if v.modified_since_load() => Some((p.id, v.path().to_path_buf())),
                _ => None,
            })
            .filter(|(id, _)| !self.preview_loads.contains(id))
            .collect();
        let images = self.env.graphics.enabled();
        for (id, path) in reloads {
            self.preview_loads.insert(id);
            let started = self.background(
                move || crate::preview::Loaded::read(&path, images),
                move |server, loaded| {
                    server.preview_loads.remove(&id);
                    if let Some(crate::pane::Content::Preview(p)) = server.panes.get_mut(&id).map(|p| &mut p.content) {
                        p.apply(loaded);
                        server.mark_dirty();
                    }
                },
            );
            // No thread to read it: the next tick tries again.
            if !started {
                self.preview_loads.remove(&id);
            }
        }
    }

    fn handle(&mut self, ev: Event) {
        // Typed bytes say themselves whether they changed anything (`client_input`).
        if !matches!(ev, Event::PaneReady(_) | Event::ClientFrame(_, crate::protocol::BYTES, _)) {
            self.chrome_changed();
        }
        match ev {
            Event::PaneReady(id) => {
                self.wake();
                if self.ready_set.insert(id) {
                    self.ready.push_back(id);
                }
            }
            Event::PaneExited(id, pid, code) => {
                self.wake();
                self.exiting.insert(id, (pid, code, Instant::now() + Duration::from_secs(1)));
                self.finish_exits();
                // What ended may have committed, checked out or merged.
                self.git_soon_everywhere();
            }
            Event::PaneFailed(id, process, why) => self.pane_failed(id, &process, &why),
            Event::Connection(stream) => {
                self.contact();
                self.new_connection(stream)
            }
            Event::ClientFrame(id, kind, payload) => {
                self.contact();
                self.client_frame(id, kind, payload)
            }
            Event::Watched(changes) => self.watched(&changes),
            Event::ClientGone(id) => self.drop_client(id),
            Event::Git(dir, info) => self.git_result(dir, info),
            Event::Warning(w) => self.warnings.push(w),
            Event::Emit(ty, fields) => self.emit(ty, fields),
            Event::Job(id, result) => {
                let done = self.jobs.remove(&id);
                match result {
                    Ok(value) => {
                        if let Some(done) = done {
                            done(self, value);
                        }
                    }
                    Err(panic) => self.warnings.push(format!("a background job failed: {}", panic_text(&*panic))),
                }
            }
            Event::RemoteAgents(conn, list) => self.remote_list(conn, list),
            Event::Wake => {
                // A preview's file changed, or a file listing is ready.
                self.check_previews();
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

    /// A client said something. After a quiet spell, the tick runs first: what it is about to
    /// act on (the processes in the panes, the definitions, the contexts read when they need
    /// to be) is as fresh as when the server ticks all the time.
    fn contact(&mut self) {
        let now = Instant::now();
        self.last_contact = now;
        if self.idle {
            self.idle = false;
            self.last_tick = now;
            self.tick(now);
        }
    }

    /// The watcher saw changes: in repositories (git is asked again), in the directories panes
    /// are in (their Terraform workspace is read again), in the configuration's directories or
    /// the accounts' usage (read again).
    fn watched(&mut self, changes: &[crate::watch::Change]) {
        let touched = self.dirwatch.changed(changes, Instant::now());
        if !touched.dirs.is_empty() {
            self.tf_touched(touched.dirs);
            self.scan_polled = true;
        }
        if touched.config {
            self.scan_definitions();
            self.reload_usage();
            self.check_limits();
        }
    }

    fn pane_output(&mut self, id: PaneId, data: &[u8]) {
        self.note_ports(id, data);
        if let Some(out) = self.run_output.get_mut(&id) {
            out.feed(data);
        }
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let first = pane.prints(data.len());
        let signals = pane.feed(data);
        if pane.has_image_work() {
            self.prepare_images();
        }
        self.pane_signals(id, signals);
        self.mark_dirty_pane(id);
        // A program's first picture is no stream of output to pace: it is drawn at once.
        if first && self.on_screen(id) {
            self.render_now();
        }
        if !self.echo.is_empty()
            && let Some(until) = self.echo.remove(&id)
            && Instant::now() <= until
        {
            self.render_now();
        }
        // Once per resize: what the pane prints after that is paced like any output.
        if !self.redraws.is_empty()
            && let Some(until) = self.redraws.remove(&id)
            && Instant::now() <= until
        {
            self.render_now();
        }
    }

    /// Exits whose last output has been read (its reader saw the end of it), or that waited
    /// long enough: a program left in the background can keep the terminal open, and print
    /// without end. A little of what is left is read each time.
    fn finish_exits(&mut self) {
        let ids: Vec<PaneId> = self.exiting.keys().copied().collect();
        let mut data = std::mem::take(&mut self.output_buf);
        for id in ids {
            let mut left: usize = 64 * 1024;
            while left > 0 && self.take_output(id, &mut data, false) {
                self.pane_output(id, &data);
                left = left.saturating_sub(data.len());
            }
            let Some(&(pid, code, until)) = self.exiting.get(&id) else { continue };
            if self.panes.get(&id).is_none_or(|p| p.output_finished()) || Instant::now() >= until {
                self.exiting.remove(&id);
                self.unparsed.remove(&id);
                self.pane_exited(id, pid, code);
            }
        }
        self.output_buf = data;
    }

    /// Act on what feeding output to pane `id` asked for.
    fn pane_signals(&mut self, id: PaneId, signals: Vec<PaneSignal>) {
        // A title, a bell, a command ended, contexts reported: shown around the panes.
        if !signals.is_empty() {
            self.chrome_changed();
        }
        for s in signals {
            match s {
                PaneSignal::Notification { title, body } => {
                    let title = if title.is_empty() { self.pane_label(id) } else { title };
                    self.notify(&title, &body, Some(id));
                }
                PaneSignal::Clipboard(text) => {
                    self.keep_copy(&text);
                    // What a pane copies goes to one clipboard: that of the client looking at its
                    // workspace that was used last (another machine attached elsewhere keeps its own).
                    let bytes = crate::pane::osc52(&text);
                    let ws = self.workspace_of(id).map(|(wi, _)| self.workspaces[wi].id);
                    if let Some(c) = self
                        .clients
                        .values()
                        .filter(|c| c.attached && Some(c.workspace) == ws)
                        .max_by_key(|c| c.last_input)
                    {
                        let _ = c.out.try_send(frame_bytes(&bytes));
                    }
                }
                PaneSignal::CommandDone(done) => self.command_done(id, done),
                PaneSignal::Contexts(contexts) => self.contexts_reported(id, contexts),
                PaneSignal::Bell | PaneSignal::TitleChanged => {}
            }
        }
    }

    // ---- rendering ---------------------------------------------------------------------

    pub fn mark_dirty(&mut self) {
        for c in self.clients.values_mut() {
            c.dirty = Dirty::Yes;
        }
        self.chrome_changed();
        self.schedule_render();
    }

    pub fn mark_dirty_client(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.dirty = Dirty::Yes;
        }
        self.chrome_changed();
        self.schedule_render();
    }

    /// What the chrome shows may have changed: it is worked out again for the next frame.
    pub(super) fn chrome_changed(&mut self) {
        self.chrome_gen = self.chrome_gen.wrapping_add(1);
    }

    /// What `pane` shows changed with what it printed: a frame is due for the clients of its
    /// workspace, and every client's next frame knows which pane printed (the sidebar shows the
    /// panes of every workspace).
    fn mark_dirty_pane(&mut self, pane: PaneId) {
        let ws = self.workspace_of(pane).map(|(w, _)| self.workspaces[w].id);
        for c in self.clients.values_mut() {
            c.printed.add(pane);
            if Some(c.workspace) == ws && c.dirty == Dirty::No {
                c.dirty = Dirty::Output;
            }
        }
        if !self.printed.contains(&pane) {
            self.printed.push(pane);
        }
        self.schedule_render();
    }

    /// What the panes that printed since the last frames show around their rows may have changed
    /// with it (a title, a directory, a command that failed). The clients of their workspace are
    /// due a frame already; the sidebar lists panes of every workspace, so a client that has one
    /// is due a whole frame too, wherever it looks.
    fn show_what_output_changed(&mut self) {
        let mut changed = false;
        for pane in std::mem::take(&mut self.printed) {
            changed |= self.facts_changed(pane);
        }
        if !changed {
            return;
        }
        let with_sidebar = |c: &&Client| self.sidebar_cols(c) > 0;
        let ids: Vec<ClientId> = self.clients.values().filter(with_sidebar).map(|c| c.id).collect();
        for id in ids {
            if let Some(c) = self.clients.get_mut(&id) {
                c.dirty = Dirty::Yes;
            }
        }
    }

    /// Draw what changed: at once after a quiet spell, else when the last frame is
    /// [`FRAME_GAP`] old, or [`BUSY_FRAME_GAP`] while output keeps coming, but no later than
    /// [`STREAM_PAUSE`] after it stops (see `settle`).
    fn schedule_render(&mut self) {
        let now = Instant::now();
        self.settle = Some(now + STREAM_PAUSE);
        if self.next_render.is_some() {
            return;
        }
        self.streak = if now < self.last_frame + FRAME_GAP { self.streak.saturating_add(1) } else { 0 };
        let gap = if self.streak >= BUSY_AFTER { BUSY_FRAME_GAP } else { FRAME_GAP };
        self.next_render = Some((self.last_frame + gap).max(now));
    }

    /// Draw as soon as the event loop gets to it: what was just typed, and its echo.
    pub(super) fn render_now(&mut self) {
        self.next_render = Some(Instant::now());
    }

    fn render_clients(&mut self) {
        self.next_render = None;
        let _frame = crate::pane::Frame::start();
        // One moment for all that these frames show of the accounts' usage, however composed.
        let now = crate::accounts::now();
        self.show_what_output_changed();
        let due = |c: &&Client| c.attached && c.dirty != Dirty::No;
        let ids: Vec<ClientId> = self.clients.values().filter(due).map(|c| c.id).collect();
        let mut again = false;
        // The earliest a moving image may be drawn where it stopped.
        let mut wake: Option<Instant> = None;
        for id in ids {
            // Slow client: keep it dirty and retry on the next frame.
            if self.clients[&id].out.len() >= 8 {
                again = true;
                continue;
            }
            // The frame is composed over the last one: from what one pane printed alone when
            // nothing else can have changed, from everything otherwise.
            let Some(c) = self.clients.get_mut(&id) else { continue };
            let mut buf = std::mem::take(&mut c.frame);
            let printed = match (c.dirty, std::mem::replace(&mut c.printed, Printed::None)) {
                (Dirty::Output, Printed::One(pane)) => Some(pane),
                _ => None,
            };
            let composed = printed.and_then(|pane| self.compose_output(id, pane, now, &mut buf));
            #[cfg(debug_assertions)]
            {
                if let Some(frame) = &composed {
                    self.assert_whole(id, now, &buf, frame);
                }
                note_frame(id, composed.is_some());
            }
            let (cursor, links, kept) = match composed {
                Some(frame) => frame,
                None => self.compose(id, now, &mut buf),
            };
            // Images the client's terminal does not have yet go first.
            let (mut bytes, more) = self.graphics_updates(id);
            // Or, for a terminal that draws sixel, the images' visible parts go over the cells.
            let (erase, images, still) = self.paint_images(id, &mut buf);
            let shown = self.client_focus(id);
            let Some(c) = self.clients.get_mut(&id) else { continue };
            // A kraal that asked is told which pane it shows, when that changed.
            let tell = c.told_shown.is_some_and(|told| told != shown);
            if tell {
                let pane = shown.map(|p| p.to_string()).unwrap_or_default();
                bytes.extend_from_slice(format!("\x1b]7701;shows;{pane}\x07").as_bytes());
            }
            c.encoder.encode_with_images(&buf, &kept, cursor, &links, &erase, &images, &mut bytes);
            c.frame = buf;
            c.dirty = if more || still.is_some() { Dirty::Yes } else { Dirty::No };
            again |= more;
            if let Some(at) = still {
                wake = Some(wake.map_or(at, |w| w.min(at)));
            }
            match c.out.try_send(frame_bytes(&bytes)) {
                Ok(()) if tell => c.told_shown = Some(shown),
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    c.encoder.invalidate();
                    c.dirty = Dirty::Yes;
                    again = true;
                    if let Some(ci) = self.images.get_mut(&id) {
                        ci.forget();
                    }
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
            self.last_frame = Instant::now();
        }
        if again {
            self.next_render = Some(Instant::now() + FRAME);
        }
        // The clients that wait for it are rendered with every frame until then.
        self.image_wake = wake;
    }

    fn shutdown(&mut self) {
        self.end_edits();
        let saved = self.save_now();
        if let Err(e) = &saved {
            eprintln!("kraal: {e}");
        }
        // A client older than `reattach` says how to come back after a restart.
        let reason = if self.restarting {
            "kraal restarted: run kraal to attach again".to_string()
        } else {
            self.stop_message(&saved)
        };
        // `kraal kill` says whether this last state, the one a restart brings back, is saved,
        // and what each pane showed.
        let answer = match saved {
            Ok(()) => Ok(crate::protocol::Response::Ok),
            Err(e) => Ok(crate::protocol::Response::Text(e)),
        };
        for id in std::mem::take(&mut self.kill_waiters) {
            self.send_msg(id, &ServerMsg::Response { result: answer.clone() });
            self.clients.remove(&id);
        }
        self.interrupt_runs();
        // What the runs wrote is in their logs before the server goes.
        let (done, flushed) = crossbeam_channel::bounded(1);
        if self.run_logs.send(crate::runbooks::LogJob::Flush(done)).is_ok() {
            let _ = flushed.recv_timeout(Duration::from_secs(10));
        }
        self.hooks.stop_all();
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for id in ids {
            // On a restart, attached clients come back to the workspace they show.
            let shown = self.clients.get(&id).filter(|c| c.attached).map(|c| c.workspace);
            let reattach = shown.filter(|_| self.restarting).map(|ws| {
                let wi = self.ws_index(ws).unwrap_or(0);
                self.workspaces.get(wi).map(|w| w.name.clone()).unwrap_or_default()
            });
            self.graphics_farewell(id);
            self.send_msg(id, &ServerMsg::Exit { reason: reason.clone(), reattach });
        }
        for p in self.panes.values_mut() {
            p.kill();
        }
        self.stop_tunnels();
        self.stop_remotes();
        // Give writer threads a moment to deliver the exit messages.
        std::thread::sleep(Duration::from_millis(50));
    }

    /// What the clients are told when the server stops for good, `saved` saying whether the
    /// session was: why, and how it comes back.
    fn stop_message(&self, saved: &Result<(), String>) -> String {
        let start = match self.session.as_str() {
            "main" => "kraal".to_string(),
            session => format!("kraal -s {session}"),
        };
        match saved {
            Ok(()) => format!("stopped; {start} starts it again with its layout"),
            Err(e) => format!("stopped, but {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Preset;

    pub(super) fn client(server: &mut Server) -> (ClientId, std::os::unix::net::UnixStream) {
        let ws = server.new_workspace("test".into(), None);
        let tab = server.alloc_id();
        server.workspaces.last_mut().unwrap().tabs.push(Tab::new(tab, "test".into()));
        let (stream, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let id = server.next_id;
        server.new_connection(stream);
        let c = server.clients.get_mut(&id).unwrap();
        c.attached = true;
        c.workspace = ws;
        c.cols = 124;
        c.rows = 36;
        (id, peer)
    }

    pub(super) fn server(tx: EventSender) -> Server {
        let config = Config::default();
        let keymap = Keymap::build(&config.keys).0;
        let hint_patterns = crate::hints::Custom::build(&config.hints, &config.tools).0;
        let bottom_bar = false;
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "publication-test".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let buffers = crate::buffers::Buffers::new(config.buffers.limit, config.buffers.max_bytes());
        Server {
            session: "publication-test".into(),
            config,
            keymap,
            env,
            tx,
            panes: HashMap::new(),
            workspaces: Vec::new(),
            workspace_order: Vec::new(),
            clients: HashMap::new(),
            next_id: 1,
            primary: None,
            // A 120×40 terminal's, until a client attaches.
            layout_area: Rect::new(0, 1, 120, 39 - u16::from(bottom_bar)),
            waits: Vec::new(),
            persist_dirty: false,
            last_persist: Instant::now(),
            last_tick: Instant::now(),
            idle: false,
            scan_polled: true,
            kube_polled: false,
            last_contact: Instant::now(),
            last_scan: Instant::now() - SCAN_EVERY,
            next_render: None,
            image_wake: None,
            last_frame: Instant::now(),
            streak: 0,
            echo: HashMap::new(),
            redraws: HashMap::new(),
            settle: None,
            quitting: false,
            kill_waiters: Vec::new(),
            restarting: false,
            outer_colors: None,
            images: HashMap::new(),
            fitted: Default::default(),
            preparing: 0,
            warnings: Vec::new(),
            accounts_error: None,
            messages: Default::default(),
            def_files: Vec::new(),
            last_def_scan: Instant::now(),
            known_servers: Vec::new(),
            discovered: Default::default(),
            accounts: crate::accounts::Accounts::empty(),
            existing_checks: HashMap::new(),
            starting_workspace: None,
            usage: Default::default(),
            usage_reset: None,
            last_codex_poll: Instant::now() - Duration::from_secs(3600),
            limit_alerts: HashMap::new(),
            screen_limits: HashMap::new(),
            switches: HashMap::new(),
            pane_edits: HashMap::new(),
            observed_agents: HashMap::new(),
            limit_events: Default::default(),
            ready: Default::default(),
            ready_set: Default::default(),
            output_buf: Vec::new(),
            unparsed: HashMap::new(),
            watched: Vec::new(),
            watch_at: None,
            exiting: Default::default(),
            focus_reported: Default::default(),
            held_input: Vec::new(),
            bottom_bar,
            turn_skipped: false,
            typed_quietly: false,
            chrome_gen: 0,
            facts: Default::default(),
            printed: Vec::new(),
            tilings: Default::default(),
            broadcast_tip_shown: false,
            buffers,
            buffers_dirty: false,
            panes_closed: false,
            tunnels: Vec::new(),
            remotes: Vec::new(),
            port_candidates: HashMap::new(),
            ports: Default::default(),
            git: HashMap::new(),
            dirwatch: dirwatch::DirWatch::new(None),
            git_requests: crossbeam_channel::unbounded().0,
            launches: crossbeam_channel::unbounded().0,
            kube: HashMap::new(),
            kube_cache: Default::default(),
            tf_dirs: Default::default(),
            hint_patterns,
            subscribers: Vec::new(),
            events: Default::default(),
            announced: Default::default(),
            hooks: events::Hooks::new(Vec::new()),
            pane_states: Default::default(),
            drafts: HashMap::new(),
            plans: HashMap::new(),
            runs: Vec::new(),
            launching: Vec::new(),
            stray_groups: Vec::new(),
            run_output: HashMap::new(),
            md_sessions: HashMap::new(),
            jobs: HashMap::new(),
            edits: Vec::new(),
            preview_loads: Default::default(),
            pending_opens: Vec::new(),
            persister: crossbeam_channel::unbounded().0,
            persisting: Default::default(),
            persisted: None,
            scrollback: HashMap::new(),
            unrestored: Vec::new(),
            deleted_workspaces: Vec::new(),
            removed_sources: Vec::new(),
            scrollback_tag: format!("{}-{}", crate::accounts::now(), std::process::id()),
            run_logs: crossbeam_channel::unbounded().0,
        }
    }
    #[test]
    fn a_folded_pane_opens_when_it_gets_the_focus() {
        let mut tab = Tab::new(1, String::new());
        tab.root = Some(Node::Split {
            axis: crate::layout::Axis::Row,
            children: vec![(Node::Leaf(1), 1.0), (Node::Stack { panes: vec![2, 3, 4], open: 2 }, 1.0)],
        });
        let open = |tab: &Tab| tab.root.as_ref().map(|r| r.holder(4));
        tab.set_focus(4);
        assert_eq!(open(&tab), Some(4));
        // Away from the stack, it stays as it was.
        tab.set_focus(1);
        assert_eq!(open(&tab), Some(4));
        tab.set_focus(3);
        assert_eq!(open(&tab), Some(3));
        // The focus goes back to the pane focused before, which opens.
        tab.set_focus(2);
        assert!(tab.remove(2));
        assert_eq!((tab.focus, open(&tab)), (Some(3), Some(3)));
        tab.set_focus(4);
        tab.set_focus(1);
        assert!(tab.remove(1));
        assert_eq!((tab.focus, open(&tab)), (Some(4), Some(4)));
    }

    #[test]
    fn a_frame_knows_whether_one_pane_alone_printed() {
        let mut printed = Printed::None;
        printed.add(4);
        printed.add(4);
        assert_eq!(printed, Printed::One(4));
        printed.add(5);
        assert_eq!(printed, Printed::Several);
        // The first one printing again changes nothing: another did meanwhile.
        printed.add(4);
        assert_eq!(printed, Printed::Several);
    }

    #[test]
    fn every_printed_pane_refreshes_its_facts() {
        let (tx, _rx) = queue::channel(16);
        let mut server = server(tx);
        let _batch = crate::pane::batch();
        let panes: Vec<_> = (0..2)
            .map(|_| {
                let spec = crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() };
                let id = server.spawn_pane(None, spec).unwrap();
                assert!(server.facts_changed(id));
                id
            })
            .collect();
        for &id in &panes {
            server.pane_output(id, format!("\x1b]2;pane-{id}\x07").as_bytes());
            assert_eq!(server.panes[&id].display_name(), format!("pane-{id}"));
        }
        let _frame = crate::pane::Frame::start();
        server.show_what_output_changed();
        assert!(server.printed.is_empty());
        // A change in the first pane must not stop the next pane's facts from being refreshed.
        for id in panes {
            assert!(!server.facts_changed(id), "pane {id}'s facts were not refreshed");
        }
    }

    #[test]
    fn a_tab_keeps_its_arrangement_as_panes_go() {
        let mut tab = Tab::new(1, String::new());
        tab.root = Some(Node::Leaf(1));
        for p in [2, 3, 4] {
            tab.root.as_mut().unwrap().split(p - 1, crate::layout::Axis::Row, p);
        }
        tab.set_focus(4);
        tab.arrange(Preset::MainLeftStacked);
        let root = tab.root.clone().unwrap();
        assert_eq!(root.stack_of(2), Some((&[2, 3, 4][..], 4)));
        assert!(tab.remove(3));
        assert_eq!(tab.root, Preset::MainLeftStacked.build(&[1, 2, 4], Some(4)));
        // With no arrangement kept, the space of a pane that goes is its neighbour's.
        tab.preset = None;
        assert!(tab.remove(1));
        assert_eq!(tab.root.as_ref().and_then(|r| r.stack_of(2)), Some((&[2, 4][..], 4)));
    }
}
