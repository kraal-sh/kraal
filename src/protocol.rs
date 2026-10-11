//! Wire protocol between the `kraal` client/CLI and the session server.
//!
//! Every message is a frame: `u32` little-endian payload length, one kind byte, then the payload.
//! Kind [`JSON`] carries a serialized [`ClientMsg`] or [`ServerMsg`]; kind [`BYTES`] carries raw
//! terminal bytes (keyboard input from the client, screen updates from the server).

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::AgentState;

pub const PROTOCOL_VERSION: u32 = 2;

pub const JSON: u8 = 0;
pub const BYTES: u8 = 1;

const MAX_FRAME: usize = 64 << 20;

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    // The other side would refuse it: say so here instead of sending it.
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[4] = kind;
    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()
}

pub fn write_json<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(io::Error::other)?;
    write_frame(w, JSON, &payload)
}

/// Read one frame. `Ok(None)` on a clean EOF between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((header[4], payload)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Start an interactive session; raw input frames follow.
    Attach {
        version: u32,
        cols: u16,
        rows: u16,
        /// Workspace to show first (opened from its definition if needed).
        workspace: Option<String>,
        /// The client runs on another machine than the server (attached over ssh).
        #[serde(default)]
        remote: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Detach,
    /// One control request; the server answers with [`ServerMsg::Response`] and closes.
    Request {
        version: u32,
        request: Request,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// The interactive session ended (detach, server shutdown, …).
    Exit {
        reason: String,
    },
    Response {
        result: Result<Response, String>,
    },
    /// Open a URL on the client's machine (Ctrl+click on a link).
    Open {
        url: String,
    },
    /// Something happened in the session (to `kraal events`).
    Event {
        event: serde_json::Value,
    },
    /// Events that happened before the subscription (`--since`), several to a frame.
    Events {
        events: Vec<serde_json::Value>,
    },
}

/// Where a new pane goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// Split the target pane along its longer side.
    #[default]
    Auto,
    Right,
    Down,
    Floating,
    /// A new tab in the target workspace.
    Tab,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    ListPanes,
    ListWorkspaces,
    OpenWorkspace {
        name: String,
    },
    /// `pane` refs are an id (`"12"`), a pane name, or `"current"` (the caller's `$KRAAL_PANE`).
    NewPane {
        target: Option<String>,
        command: Vec<String>,
        cwd: Option<PathBuf>,
        name: Option<String>,
        placement: Placement,
        preview: Option<PathBuf>,
        focus: bool,
        /// Close the pane when its program exits (default: keep it, showing the exit code).
        #[serde(default)]
        close_on_exit: bool,
    },
    ClosePane {
        pane: String,
    },
    FocusPane {
        pane: String,
    },
    RenamePane {
        pane: String,
        name: String,
    },
    /// Type into a pane. A production pane is refused unless `production` says the caller
    /// knows (scripts and agents must not reach production by accident).
    SendText {
        pane: String,
        text: String,
        #[serde(default)]
        production: bool,
    },
    SendKeys {
        pane: String,
        keys: Vec<String>,
        #[serde(default)]
        production: bool,
    },
    ReadPane {
        pane: String,
        /// Number of trailing lines, scrollback included; `None` = visible screen.
        lines: Option<usize>,
    },
    /// Block until the pane matches one of `until` (`working`, `blocked`, `done`, `idle`,
    /// `exit`, `quiet`) or the timeout expires.
    Wait {
        pane: String,
        until: Vec<String>,
        timeout_ms: Option<u64>,
    },
    /// Sent by agent hooks running inside a pane.
    AgentReport {
        pane: String,
        agent: String,
        state: Option<AgentState>,
        session_id: Option<String>,
        message: Option<String>,
    },
    Notify {
        title: String,
        body: String,
    },
    /// Prepare a step runbook: the full plan to review, and a token to run exactly that plan
    /// (the file as read now, the targets and parameters resolved now). `params` are
    /// `NAME=value`; the others take their default. `pane` gives the context.
    RunbookPlan {
        path: PathBuf,
        target: Option<String>,
        params: Vec<(String, String)>,
        pane: Option<String>,
    },
    /// Run the plan a [`Request::RunbookPlan`] prepared; a token works once, for 10 minutes.
    RunbookRun {
        token: String,
    },
    /// Stream events (`pane.exited`, `agent.*`…; all when empty) until the client goes away,
    /// after those numbered above `since` that the server still has.
    Subscribe {
        events: Vec<String>,
        #[serde(default)]
        since: Option<u64>,
        /// The `boot` of the server the cursor `since` comes from.
        #[serde(default)]
        boot: Option<u64>,
    },
    /// Open an SSH pane to a server (name or `user@host[:port]`), or a whole group in a grid.
    OpenSsh {
        target: String,
        group: bool,
        placement: Placement,
        /// Pane to split (its workspace receives the new pane).
        near: Option<String>,
        /// A remote command to run instead of a shell (e.g. `journalctl -fu nginx`).
        #[serde(default)]
        command: Vec<String>,
    },
    /// Start an agent (claude, codex…) in a new pane, on `account` or the one the rules pick.
    OpenAgent {
        agent: String,
        account: Option<String>,
        placement: Placement,
        near: Option<String>,
        cwd: Option<PathBuf>,
        /// Given to the agent after kraal's own arguments.
        #[serde(default)]
        args: Vec<String>,
    },
    /// Forward `remote` (`[host:]port` as seen from the server) to a local port, and keep it up.
    TunnelAdd {
        server: String,
        remote: String,
        local: Option<u16>,
    },
    /// Close a tunnel by its local port or id.
    TunnelRemove {
        key: String,
    },
    TunnelList,
    /// Open a two-panel file manager on two locations (`/path` or `server:path`).
    OpenFiles {
        left: String,
        right: String,
        near: Option<String>,
    },
    /// Write the current layout of a workspace as a KDL definition file.
    SaveWorkspace {
        workspace: Option<String>,
        name: Option<String>,
    },
    KillServer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong {
        pid: u32,
        session: String,
        version: String,
    },
    Pane(Box<PaneInfo>),
    Panes(Vec<PaneInfo>),
    Workspaces(Vec<WorkspaceInfo>),
    Text(String),
    /// A runbook's plan, and the token that runs it.
    Plan {
        text: String,
        token: String,
    },
    Path(PathBuf),
    Tunnels(Vec<crate::server::net::TunnelInfo>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: u32,
    pub name: Option<String>,
    pub title: String,
    pub workspace: String,
    pub tab: String,
    pub kind: String,
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub agent: Option<String>,
    pub state: AgentState,
    pub exit_code: Option<i32>,
    /// The command waits for Enter (restored session, start_suspended, or it could not start).
    #[serde(default)]
    pub suspended: bool,
    /// Kitty keyboard protocol flags the program enabled (0 = legacy keys).
    #[serde(default)]
    pub keyboard: u8,
    /// The server the pane is connected to.
    #[serde(default)]
    pub server: Option<String>,
    /// Why the pane is production (`server web-prod-1`), if it is.
    #[serde(default)]
    pub production: Option<String>,
    /// Kubernetes context, Terraform workspace and AWS profile labels (`tf:prod`).
    #[serde(default)]
    pub contexts: Vec<String>,
    pub focused: bool,
    pub floating: bool,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub open: bool,
    pub definition: Option<PathBuf>,
    pub tabs: usize,
    pub panes: usize,
    pub working: usize,
    pub blocked: usize,
    pub done: usize,
}
