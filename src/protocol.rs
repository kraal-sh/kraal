//! Wire protocol between the `kraal` client/CLI and the session server.
//!
//! Every message is a frame: `u32` little-endian payload length, one kind byte, then the payload.
//! Kind [`JSON`] carries a serialized [`ClientMsg`] or [`ServerMsg`]; kind [`BYTES`] carries raw
//! terminal bytes (keyboard input from the client, screen updates from the server). A frame of
//! another kind is skipped.
//!
//! # Compatibility
//!
//! A kraal works with sessions started by another release, older or newer, from the oldest
//! supported release on (0.2.0, `RELEASES` in tests/compat.sh, which tests it): an update never
//! forces a restart (docs/compatibility.md). The protocol changes only in these ways:
//!
//! - A new field is optional: it has `#[serde(default)]`, so that a message from a peer that
//!   does not send it still reads, and every reader ignores fields it does not know (never
//!   `deny_unknown_fields`). A field never changes type or meaning; a unit answer kind never
//!   gets data; a request kind, message type or [`Feature`] name is never reused.
//! - A new request or client message is refused by a server that does not know it, with an
//!   error that starts with [`UNSUPPORTED`] (`bad message` from older releases), and the client
//!   then says which release the session runs and that `kraal restart` updates it. A request
//!   that an older server would take and quietly do less of (it drops an option it does not
//!   know) needs a [`Feature`] with `check_first`: the client asks the server before sending it.
//! - A server lists its [`Feature`]s in [`Response::Pong`] and [`ServerMsg::Attached`]; one that
//!   lists none is older than these lists. A client lists in its messages what it can read
//!   beyond the oldest supported release ([`CLIENT_FEATURES`]).
//! - A new server message type goes to attached clients only (every release skips the ones it
//!   does not know there), a new answer kind only answers a new request, and a new value of an
//!   enum in an answer goes only to clients that list [`LENIENT`]: older ones fail on it. Every
//!   message and enum read here takes what it does not know as a fallback (an `Unknown`
//!   variant, `#[serde(other)]`), never as an error.
//! - [`PROTOCOL_VERSION`] changes only for what these rules cannot express. Peers of different
//!   protocol versions refuse each other; `kraal restart` and `kraal kill` still stop the
//!   server, whatever its version.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::AgentState;

/// The protocol's generation: what the rules above cannot express changes it, and peers of
/// different generations refuse each other.
pub const PROTOCOL_VERSION: u32 = 2;

/// This kraal's release.
pub const KRAAL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Something a server can do that one of the oldest supported release cannot. A server lists
/// the names of those it has in [`Response::Pong`] and [`ServerMsg::Attached`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feature {
    /// Its name in the server's list.
    pub name: &'static str,
    /// What it lets a client do, as said to the user ("which cannot …").
    pub does: &'static str,
    /// A server without it would take the request and do less (an option it does not know is
    /// dropped), rather than refuse it: the client asks before sending.
    pub check_first: bool,
}

pub const LIST_AGENTS: Feature =
    Feature { name: "list_agents", does: "list the agents of the sessions on your servers", check_first: false };
pub const BUFFERS: Feature = Feature { name: "buffers", does: "keep paste buffers", check_first: false };
pub const EDIT_SUDO: Feature = Feature { name: "edit_sudo", does: "edit a server's files as root", check_first: true };

pub const ACCOUNT_IDS: Feature =
    Feature { name: "account_ids", does: "select an account by its identity", check_first: true };
pub const NATIVE_AGENTS: Feature =
    Feature { name: "native_agents", does: "start agents with native account management", check_first: true };
pub const STATE_V2: Feature =
    Feature { name: "state_v2", does: "save sessions in the version 2 format", check_first: false };
/// What this kraal's server lists.
pub const WORKSPACE_FILES: Feature =
    Feature { name: "workspace_files", does: "preview imports and export layouts safely", check_first: false };
pub const WORKSPACE_LIFECYCLE: Feature = Feature {
    name: "workspace_lifecycle",
    does: "create, rename, close, delete and restore workspaces",
    check_first: false,
};
pub const CLIPBOARD: Feature =
    Feature { name: "clipboard", does: "copy through the client clipboard policy", check_first: false };
pub const ATTACHMENTS: Feature = Feature { name: "attachments", does: "attach local files", check_first: false };
pub const PASTED_PATHS: Feature =
    Feature { name: "pasted_paths", does: "recognize laptop paths in pastes", check_first: false };
pub const CLIPBOARD_IMAGES: Feature =
    Feature { name: "clipboard_images", does: "attach clipboard images", check_first: false };
pub const AGENT_CLIPBOARD: Feature =
    Feature { name: "agent_clipboard", does: "paste laptop clipboard images into agents", check_first: false };
pub const FEATURES: [Feature; 13] = [
    LIST_AGENTS,
    BUFFERS,
    EDIT_SUDO,
    ACCOUNT_IDS,
    NATIVE_AGENTS,
    STATE_V2,
    WORKSPACE_FILES,
    WORKSPACE_LIFECYCLE,
    CLIPBOARD,
    ATTACHMENTS,
    PASTED_PATHS,
    CLIPBOARD_IMAGES,
    AGENT_CLIPBOARD,
];

/// The names of this kraal's server features, as it lists them.
pub fn feature_names() -> Vec<String> {
    FEATURES.iter().map(|f| f.name.to_string()).collect()
}

/// Said by a client that reads message types, answer kinds and enum values it does not know as
/// their fallback (see the rules above) instead of failing on them.
pub const LENIENT: &str = "lenient";

/// What this kraal's client can read beyond what one of the oldest supported release can. It
/// lists them in its messages, for a later server to rely on.
pub const CLIENT_FEATURES: [&str; 6] =
    [LENIENT, CLIPBOARD.name, ATTACHMENTS.name, PASTED_PATHS.name, CLIPBOARD_IMAGES.name, AGENT_CLIPBOARD.name];

pub fn client_features() -> Vec<String> {
    CLIENT_FEATURES.iter().map(|f| f.to_string()).collect()
}

/// How a server begins its answer to a request or message it does not know or cannot read.
/// Servers older than this prefix answer `bad message: …`.
pub const UNSUPPORTED: &str = "unsupported request";

/// The answer to `msg` (a request or client message, as read), which this server does not know.
pub fn unsupported(msg: &serde_json::Value) -> String {
    let name = msg.get("cmd").or_else(|| msg.get("type")).and_then(|n| n.as_str()).unwrap_or("?");
    format!("{UNSUPPORTED} `{name}`: kraal {KRAAL_VERSION} does not know it")
}

/// Whether `error`, a server's answer, says that it could not read what it was sent: the client
/// is newer than the server, or speaks a request the server does not know.
pub fn refused_as_unknown(error: &str) -> bool {
    error.starts_with(UNSUPPORTED) || error.starts_with("bad message:")
}

/// Whether release `a` (`0.2.1`, `v0.3.0-rc.1`) comes after release `b`, as semver orders them:
/// a pre-release comes before its release. A version that cannot be read is never newer.
pub fn newer(a: &str, b: &str) -> bool {
    use std::cmp::Ordering;
    fn parse(v: &str) -> Option<([u64; 3], Vec<&str>)> {
        let v = v.trim();
        let v = v.strip_prefix('v').unwrap_or(v);
        let v = v.split('+').next().unwrap_or(v);
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, pre.split('.').collect()),
            None => (v, Vec::new()),
        };
        let mut nums = [0u64; 3];
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() > 3 {
            return None;
        }
        for (n, p) in nums.iter_mut().zip(parts) {
            *n = p.parse().ok()?;
        }
        Some((nums, pre))
    }
    let (Some((a_nums, a_pre)), Some((b_nums, b_pre))) = (parse(a), parse(b)) else { return false };
    let pre = || {
        if a_pre.is_empty() || b_pre.is_empty() {
            // The release comes after its pre-releases.
            return b_pre.len().min(1).cmp(&a_pre.len().min(1));
        }
        for (x, y) in a_pre.iter().zip(&b_pre) {
            let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            };
            if order != Ordering::Equal {
                return order;
            }
        }
        a_pre.len().cmp(&b_pre.len())
    };
    a_nums.cmp(&b_nums).then_with(pre) == Ordering::Greater
}

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
    // A small frame (a key) goes in one write: the reader is not woken by a header alone.
    if payload.len() <= 4096 {
        let mut frame = Vec::with_capacity(header.len() + payload.len());
        frame.extend_from_slice(&header);
        frame.extend_from_slice(payload);
        w.write_all(&frame)?;
    } else {
        w.write_all(&header)?;
        w.write_all(payload)?;
    }
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
        /// A pane to show first (an id or a name), in place of the workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        /// The release of the kraal installed on the server's machine, as the client knows it
        /// (a local client is that binary): the server says when it is newer than its own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        installed: Option<String>,
        /// What the client can read ([`CLIENT_FEATURES`]).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Detach,
    /// Reserve the destination while a negotiated client classifies a buffered paste.
    PasteStart,
    AttachmentRequest {
        token: String,
        /// Paste content, or the original shortcut key when `image` and `pane` are set.
        text: Vec<u8>,
        #[serde(default)]
        image: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<crate::layout::PaneId>,
    },
    AttachmentText {
        token: String,
        text: Vec<u8>,
    },
    AttachmentStart {
        token: String,
        files: Vec<crate::attachments::FileSpec>,
    },
    AttachmentChunk {
        token: String,
        file: usize,
        offset: u64,
        data: String,
    },
    AttachmentFinish {
        token: String,
    },
    AttachmentCancel {
        token: String,
    },
    AttachmentStatus {
        token: String,
    },
    AttachmentDismiss {
        token: String,
    },
    AttachmentLocal {
        token: String,
        paths: Vec<String>,
    },
    ClipboardResult {
        id: u64,
        outcome: ClipboardOutcome,
        #[serde(default)]
        native_unavailable: bool,
    },
    /// One control request; the server answers with [`ServerMsg::Response`] and closes.
    Request {
        version: u32,
        request: Request,
        /// What the client can read ([`CLIENT_FEATURES`]).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
    },
    /// A message from a newer client, or one this kraal cannot read (only ever read).
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ClientMsg {
    /// `request`, as this kraal sends it.
    pub fn request(request: Request) -> ClientMsg {
        ClientMsg::Request { version: PROTOCOL_VERSION, request, features: client_features() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// The interactive session ended (detach, server shutdown, …).
    Exit {
        reason: String,
        /// The server restarts (`kraal restart`): the workspace the client showed, to attach to
        /// again once the next server listens.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reattach: Option<String>,
    },
    /// The first answer to an attach: the server's kraal release. A server that does not send
    /// it is older than this message.
    Attached {
        version: String,
        /// Its [`Feature`]s; `None` from a server older than this list.
        #[serde(default)]
        features: Option<Vec<String>>,
    },
    Response {
        result: Result<Response, String>,
    },
    ClipboardFocus {
        pane: Option<crate::layout::PaneId>,
        agent: bool,
    },
    AttachmentPrepared {
        token: String,
        target: String,
        limits: crate::attachments::Settings,
        broadcast: bool,
    },
    ClipboardImageOffer {
        token: String,
        target: String,
        limits: crate::attachments::Settings,
    },
    AttachmentOffer {
        token: String,
        target: String,
        limits: crate::attachments::Settings,
    },
    AttachmentAck {
        token: String,
        received: u64,
    },
    AttachmentResult {
        #[serde(default)]
        kind: crate::theme::NoticeKind,
        token: String,
        message: String,
        #[serde(default)]
        pasted: bool,
    },
    Clipboard {
        id: u64,
        origin: ClipboardOrigin,
        text: String,
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
    /// A message from a newer server, or one this kraal cannot read (only ever read): skipped.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ServerMsg {
    /// The end of an interactive session that does not come back by itself.
    pub fn exit(reason: impl Into<String>) -> ServerMsg {
        ServerMsg::Exit { reason: reason.into(), reattach: None }
    }
}

/// Unknown origins never gain permission to write the native clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardOrigin {
    User,
    Program,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardOutcome {
    Native,
    NativeStarted,
    Terminal,
    Disabled,
    TooLarge,
    Busy,
    Failed,
    #[serde(other)]
    Unknown,
}

impl ClipboardOutcome {
    pub fn message(self) -> &'static str {
        match self {
            Self::Native => "copied to laptop clipboard",
            Self::NativeStarted => "copy handed to native clipboard owner",
            Self::Terminal => "copy sent to terminal via OSC 52",
            Self::Disabled => "external copy disabled; use paste buffers",
            Self::TooLarge => "copy too large; raise clipboard.max_bytes and attach again",
            Self::Busy => "copy busy: try again",
            Self::Failed | Self::Unknown => "copy not completed: try again",
        }
    }
}

/// Where a new pane goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Right,
    Down,
    Floating,
    /// A new tab in the target workspace.
    Tab,
    /// Split the target pane along its longer side. Also a placement this kraal does not know.
    #[default]
    #[serde(other)]
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    ListPanes,
    /// The panes running an agent, then the agents of the servers' sessions watched (those
    /// have `session`).
    ListAgents,
    ListWorkspaces,
    ListDeletedWorkspaces,
    CreateWorkspace {
        name: String,
        cwd: PathBuf,
    },
    RenameWorkspace {
        workspace: String,
        name: String,
    },
    CloseWorkspace {
        workspace: String,
        expected: Option<String>,
    },
    DeleteWorkspace {
        workspace: String,
        expected: Option<String>,
    },
    RestoreWorkspace {
        workspace: String,
        name: Option<String>,
    },
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
        /// A state this kraal does not know (from a newer hook) reads as none: it changes
        /// nothing, where `unknown` would end the agent's session.
        #[serde(default, deserialize_with = "known_state")]
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
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
    /// Close a tunnel by its local port.
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
    /// Edit a server's file (`server:path`) in the local editor, in a floating pane; each save
    /// goes back to the server. A production server is refused unless `production` says the
    /// caller knows. With `sudo`, as root through sudo there (a pane asks for the password).
    /// Answered once the editor is open.
    EditRemote {
        target: String,
        line: Option<u32>,
        near: Option<String>,
        #[serde(default)]
        production: bool,
        #[serde(default)]
        sudo: bool,
    },
    /// Write the current layout of a workspace as a KDL definition file.
    SaveWorkspace {
        workspace: Option<String>,
        name: Option<String>,
    },
    ImportWorkspace {
        path: PathBuf,
        name: Option<String>,
        expected: Option<crate::paths::FileRevision>,
        #[serde(default)]
        accounts: Vec<(String, String)>,
    },
    ExportWorkspace {
        workspace: Option<String>,
        #[serde(default)]
        identity: Option<String>,
        path: PathBuf,
        expected: Option<crate::paths::FileRevision>,
    },
    /// Publish a batch import while the server holds workspace ownership stable.
    PublishWorkspaceFile {
        path: PathBuf,
        source: String,
        expected: crate::paths::FileRevision,
    },
    /// The paste buffers, newest first.
    BufferList,
    /// A buffer's text: by number (0 is the newest numbered buffer) or name; the newest without.
    BufferShow {
        buffer: Option<String>,
    },
    /// Keep `text` in the buffer `name`, or in a new numbered buffer without one.
    BufferSet {
        name: Option<String>,
        text: String,
    },
    /// Paste a buffer (the newest without one) into a pane as a terminal pastes: bracketed when
    /// the program asked for it. A production pane is refused unless `production` says the
    /// caller knows.
    BufferPaste {
        buffer: Option<String>,
        pane: String,
        #[serde(default)]
        production: bool,
    },
    /// Delete a buffer (the newest without one), or all of them.
    BufferDelete {
        buffer: Option<String>,
        #[serde(default)]
        all: bool,
    },
    /// Read the configuration file again; answered with its warnings, one per line.
    ReloadConfig,
    /// Save the session and stop. With `restart`, attached clients wait for the next server
    /// (older servers read it as a plain stop).
    KillServer {
        #[serde(default)]
        restart: bool,
    },
    /// A request from a newer client, or one this kraal cannot read (only ever read): refused
    /// with [`UNSUPPORTED`].
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl Request {
    /// The [`Feature`] a server needs for this request, beyond what every server since the
    /// oldest supported release does.
    pub fn needs(&self) -> Option<Feature> {
        match self {
            Request::ImportWorkspace { .. }
            | Request::ExportWorkspace { .. }
            | Request::PublishWorkspaceFile { .. } => Some(WORKSPACE_FILES),
            Request::ListAgents => Some(LIST_AGENTS),
            Request::OpenAgent { agent, .. }
                if crate::agent_registry::find(agent).is_some_and(|a| a.accounts.is_none()) =>
            {
                Some(NATIVE_AGENTS)
            }
            Request::OpenAgent { account_id: Some(_), .. } => Some(ACCOUNT_IDS),
            Request::BufferList
            | Request::BufferShow { .. }
            | Request::BufferSet { .. }
            | Request::BufferPaste { .. }
            | Request::BufferDelete { .. } => Some(BUFFERS),
            Request::EditRemote { sudo: true, .. } => Some(EDIT_SUDO),
            // An older server reads a restart as a stop, and `kraal restart` starts the next one.
            Request::KillServer { .. } => None,
            Request::ListDeletedWorkspaces
            | Request::CreateWorkspace { .. }
            | Request::RenameWorkspace { .. }
            | Request::CloseWorkspace { .. }
            | Request::DeleteWorkspace { .. }
            | Request::RestoreWorkspace { .. } => Some(WORKSPACE_LIFECYCLE),
            Request::Ping
            | Request::ListPanes
            | Request::ListWorkspaces
            | Request::OpenWorkspace { .. }
            | Request::NewPane { .. }
            | Request::ClosePane { .. }
            | Request::FocusPane { .. }
            | Request::RenamePane { .. }
            | Request::SendText { .. }
            | Request::SendKeys { .. }
            | Request::ReadPane { .. }
            | Request::Wait { .. }
            | Request::AgentReport { .. }
            | Request::Notify { .. }
            | Request::RunbookPlan { .. }
            | Request::RunbookRun { .. }
            | Request::Subscribe { .. }
            | Request::OpenSsh { .. }
            | Request::OpenAgent { .. }
            | Request::TunnelAdd { .. }
            | Request::TunnelRemove { .. }
            | Request::TunnelList
            | Request::OpenFiles { .. }
            | Request::EditRemote { .. }
            | Request::SaveWorkspace { .. }
            | Request::ReloadConfig
            | Request::Unknown(_) => None,
        }
    }

    /// Its name on the wire (`list_panes`).
    pub fn name(&self) -> String {
        let v = serde_json::to_value(self).unwrap_or_default();
        v.get("cmd").and_then(|c| c.as_str()).unwrap_or("?").to_string()
    }
}

/// A reported agent state, `None` for one this kraal does not know.
fn known_state<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<AgentState>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.as_ref().and_then(|v| v.as_str()).and_then(AgentState::parse))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong {
        pid: u32,
        session: String,
        version: String,
        /// Its [`Feature`]s; `None` from a server older than this list.
        #[serde(default)]
        features: Option<Vec<String>>,
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
    WorkspacePreview {
        workspace: String,
        revision: String,
        question: Option<String>,
    },
    ImportPreview(crate::workspace::ImportPreview),
    ExportPreview {
        identity: String,
        path: PathBuf,
        revision: crate::paths::FileRevision,
        question: Option<String>,
    },
    Tunnels(Vec<crate::server::net::TunnelInfo>),
    Buffers(Vec<BufferInfo>),
    /// An answer from a newer server, or one this kraal cannot read (only ever read).
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

/// A paste buffer, as `kraal buffer list` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferInfo {
    /// What `kraal buffer` commands take: the number of a numbered buffer (0 is the newest), or
    /// the name of a named one.
    pub buffer: String,
    pub named: bool,
    pub bytes: usize,
    pub lines: usize,
    /// When it was copied or set, in seconds since the epoch.
    pub at: i64,
    /// Its text on one line, cut short, control characters made visible.
    pub preview: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
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
    /// For an agent of a kraal session on a server (`server`): that session. `id` is then its
    /// pane's id there, and `kind` is `remote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub open: bool,
    pub definition: Option<PathBuf>,
    pub tabs: usize,
    pub panes: usize,
    pub working: usize,
    pub blocked: usize,
    pub done: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_lifecycle_requests_advertise_their_feature_and_default_to_preview() {
        for json in [
            r#"{"cmd":"list_deleted_workspaces"}"#,
            r#"{"cmd":"create_workspace","name":"api","cwd":"/tmp"}"#,
            r#"{"cmd":"rename_workspace","workspace":"api","name":"backend"}"#,
            r#"{"cmd":"close_workspace","workspace":"api"}"#,
            r#"{"cmd":"delete_workspace","workspace":"api"}"#,
            r#"{"cmd":"restore_workspace","workspace":"api"}"#,
        ] {
            let request: Request = serde_json::from_str(json).unwrap();
            assert_eq!(request.needs(), Some(WORKSPACE_LIFECYCLE));
            let copy: Request = serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
            assert_eq!(copy.name(), request.name());
            match request {
                Request::CloseWorkspace { expected, .. } | Request::DeleteWorkspace { expected, .. } => {
                    assert!(expected.is_none())
                }
                _ => {}
            }
        }
        assert!(feature_names().contains(&WORKSPACE_LIFECYCLE.name.to_string()));
    }

    #[test]
    fn workspace_identity_is_optional_for_older_servers_and_clients() {
        let old = serde_json::json!({ "name": "w", "open": true, "definition": null,
            "tabs": 1, "panes": 2, "working": 0, "blocked": 0, "done": 0 });
        let mut workspace: WorkspaceInfo = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(workspace.id, None);
        assert_eq!(serde_json::to_value(&workspace).unwrap(), old);
        workspace.id = Some("stable".into());
        let mut new = serde_json::to_value(&workspace).unwrap();
        assert_eq!(new.as_object_mut().unwrap().remove("id"), Some(serde_json::json!("stable")));
        assert_eq!(new, old, "the fields an old client reads have not changed");
    }

    #[test]
    fn releases_compare_as_semver() {
        assert!(newer("0.2.1", "0.2.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("v1.0.0", "0.99.0"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.2.0", "0.2.1"));
        assert!(newer("0.3.0", "0.3.0-rc.1"));
        assert!(!newer("0.3.0-rc.1", "0.3.0"));
        assert!(newer("0.3.0-rc.10", "0.3.0-rc.9"));
        assert!(newer("0.3.0-rc.1", "0.3.0-beta.2"));
        assert!(newer("0.3.0-rc.1.1", "0.3.0-rc.1"));
        assert!(newer("0.2.1+build.5", "0.2.0"));
        assert!(newer("0.3", "0.2.9"));
        assert!(!newer("garbage", "0.2.0") && !newer("0.2.0", "garbage") && !newer("", ""));
        assert!(!newer(KRAAL_VERSION, KRAAL_VERSION));
    }

    #[test]
    fn older_and_newer_peers_read_what_they_know() {
        // A kraal before these fields reads the messages as it always did.
        #[derive(Deserialize)]
        #[serde(tag = "cmd", rename_all = "snake_case")]
        enum OldRequest {
            KillServer,
        }
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum OldServerMsg {
            Exit { reason: String },
        }
        let kill = serde_json::to_string(&Request::KillServer { restart: true }).unwrap();
        assert!(matches!(serde_json::from_str::<OldRequest>(&kill), Ok(OldRequest::KillServer)), "{kill}");
        let exit =
            serde_json::to_string(&ServerMsg::Exit { reason: "bye".into(), reattach: Some("w".into()) }).unwrap();
        assert!(
            matches!(serde_json::from_str::<OldServerMsg>(&exit), Ok(OldServerMsg::Exit { reason }) if reason == "bye")
        );
        let attached =
            serde_json::to_string(&ServerMsg::Attached { version: "0.2.1".into(), features: Some(feature_names()) })
                .unwrap();
        assert!(attached.starts_with(r#"{"type":"attached","version":"0.2.1","features":["#), "{attached}");
        assert!(serde_json::from_str::<OldServerMsg>(&attached).is_err(), "an older client skips it");
        // And this kraal reads what older ones send.
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"cmd":"kill_server"}"#),
            Ok(Request::KillServer { restart: false })
        ));
        assert!(matches!(
            serde_json::from_str::<ServerMsg>(r#"{"type":"exit","reason":"detached"}"#),
            Ok(ServerMsg::Exit { reattach: None, .. })
        ));
        assert!(matches!(
            serde_json::from_str::<ServerMsg>(r#"{"type":"attached","version":"0.2.0"}"#),
            Ok(ServerMsg::Attached { features: None, .. })
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"attachment_request","token":"ab","text":[]}"#),
            Ok(ClientMsg::AttachmentRequest { image: false, pane: None, .. })
        ));
        let hello = r#"{"type":"attach","version":2,"cols":80,"rows":24,"workspace":null,"remote":false}"#;
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(hello),
            Ok(ClientMsg::Attach { installed: None, ref features, .. }) if features.is_empty()
        ));
    }

    /// What a server of the oldest supported release (0.2.0) sends, as it sends it.
    #[test]
    fn answers_of_the_oldest_supported_release_read() {
        let pong = r#"{"type":"response","result":{"Ok":{"kind":"pong","data":{"pid":7,"session":"main","version":"0.2.0"}}}}"#;
        let Ok(ServerMsg::Response { result: Ok(Response::Pong { pid: 7, features: None, .. }) }) =
            serde_json::from_str::<ServerMsg>(pong)
        else {
            panic!("{pong}")
        };
        let pane = r#"{"id":3,"name":null,"title":"sh","workspace":"w","tab":"t","kind":"shell","command":[],"cwd":null,"agent":"claude","state":"working","exit_code":null,"focused":true,"floating":false,"cols":80,"rows":24}"#;
        let panes = format!(r#"{{"type":"response","result":{{"Ok":{{"kind":"panes","data":[{pane}]}}}}}}"#);
        let Ok(ServerMsg::Response { result: Ok(Response::Panes(list)) }) = serde_json::from_str::<ServerMsg>(&panes)
        else {
            panic!("{panes}")
        };
        assert_eq!((list[0].id, list[0].state, list[0].session.as_deref()), (3, AgentState::Working, None));
        let refused = r#"{"type":"response","result":{"Err":"bad message: unknown variant `buffer_list`, expected one of `ping`"}}"#;
        let Ok(ServerMsg::Response { result: Err(e) }) = serde_json::from_str::<ServerMsg>(refused) else {
            panic!("{refused}")
        };
        assert!(refused_as_unknown(&e));
        // And what such a client sends.
        let request = r#"{"type":"request","version":2,"request":{"cmd":"list_panes"}}"#;
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(request),
            Ok(ClientMsg::Request { request: Request::ListPanes, ref features, .. }) if features.is_empty()
        ));
    }

    #[test]
    fn what_this_kraal_does_not_know_is_refused_by_name() {
        let from_newer = r#"{"type":"request","version":2,"request":{"cmd":"teleport","to":"mars"},"features":["x"]}"#;
        let Ok(ClientMsg::Request { request: Request::Unknown(request), .. }) =
            serde_json::from_str::<ClientMsg>(from_newer)
        else {
            panic!("{from_newer}")
        };
        let answer = unsupported(&request);
        assert!(answer.starts_with(UNSUPPORTED) && answer.contains("`teleport`"), "{answer}");
        assert!(refused_as_unknown(&answer));
        assert!(!refused_as_unknown("no such pane: 12"));
        let Ok(ClientMsg::Unknown(msg)) = serde_json::from_str::<ClientMsg>(r#"{"type":"zoom","factor":2}"#) else {
            panic!()
        };
        assert!(unsupported(&msg).contains("`zoom`"));
        assert_eq!(Request::BufferList.name(), "buffer_list");
    }

    #[test]
    fn a_hook_state_this_kraal_does_not_know_changes_nothing() {
        let report = |state: &str| {
            let json = format!(r#"{{"cmd":"agent_report","pane":"1","agent":"claude","state":{state}}}"#);
            match serde_json::from_str::<Request>(&json) {
                Ok(Request::AgentReport { state, .. }) => state,
                other => panic!("{json}: {other:?}"),
            }
        };
        assert_eq!(report(r#""compacting""#), None);
        assert_eq!(report("null"), None);
        assert_eq!(report(r#""unknown""#), Some(AgentState::Unknown), "the agent's session ended");
        assert_eq!(report(r#""blocked""#), Some(AgentState::Blocked));
        // In an answer, it is shown as unknown.
        assert_eq!(serde_json::from_str::<AgentState>(r#""compacting""#).unwrap(), AgentState::Unknown);
        assert_eq!(serde_json::to_string(&AgentState::Unknown).unwrap(), r#""unknown""#);
    }

    #[test]
    fn native_agents_require_negotiation_but_managed_agents_keep_their_request() {
        assert!(feature_names().iter().any(|name| name == NATIVE_AGENTS.name));
        for agent in crate::agent_registry::AGENTS {
            let request = Request::OpenAgent {
                agent: agent.name.into(),
                account: None,
                account_id: None,
                placement: Placement::Auto,
                near: None,
                cwd: None,
                args: Vec::new(),
            };
            if agent.accounts.is_none() {
                assert_eq!(request.needs(), Some(NATIVE_AGENTS));
                assert!(request.needs().unwrap().check_first);
            } else {
                assert_eq!(request.needs(), None);
            }
        }
    }

    #[test]
    fn account_identity_is_optional_and_requires_a_server_that_understands_it() {
        let old = serde_json::json!({ "cmd": "open_agent", "agent": "claude", "account": "work",
            "placement": "auto", "near": null, "cwd": null });
        let request: Request = serde_json::from_value(old.clone()).unwrap();
        assert!(matches!(request, Request::OpenAgent { account_id: None, .. }));
        assert_eq!(request.needs(), None);
        let mut new = old;
        new["account_id"] = serde_json::json!("stable");
        let request: Request = serde_json::from_value(new).unwrap();
        assert_eq!(request.needs(), Some(ACCOUNT_IDS));
        assert!(request.needs().unwrap().check_first, "an old server must not silently select by a reused label");
        let wire = serde_json::to_value(request).unwrap();
        assert_eq!(wire["account"], "work");
        assert_eq!(wire["account_id"], "stable");
    }

    #[test]
    fn file_transfer_requests_require_the_new_capability_and_old_save_still_reads() {
        for value in [
            serde_json::json!({"cmd": "import_workspace", "path": "/layout.kdl", "name": null, "expected": null}),
            serde_json::json!({"cmd": "export_workspace", "path": "/layout.kdl", "workspace": null, "expected": null}),
        ] {
            let request: Request = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(request.needs(), Some(WORKSPACE_FILES));
            assert_eq!(serde_json::to_value(request).unwrap()["cmd"], value["cmd"]);
        }
        let old: Request =
            serde_json::from_value(serde_json::json!({"cmd": "save_workspace", "workspace": "api", "name": null}))
                .unwrap();
        assert!(matches!(old, Request::SaveWorkspace { .. }));
        assert_eq!(old.needs(), None);
        assert!(feature_names().contains(&WORKSPACE_FILES.name.to_string()));
    }

    #[test]
    fn requests_say_what_an_older_server_lacks() {
        assert_eq!(Request::ListAgents.needs(), Some(LIST_AGENTS));
        assert_eq!(Request::BufferDelete { buffer: None, all: true }.needs(), Some(BUFFERS));
        let edit = |sudo| Request::EditRemote { target: "a:b".into(), line: None, near: None, production: false, sudo };
        assert_eq!(edit(true).needs(), Some(EDIT_SUDO));
        assert_eq!(edit(false).needs(), None);
        assert_eq!(Request::KillServer { restart: true }.needs(), None);
        assert_eq!(Request::ListPanes.needs(), None);
        // A name is never listed twice.
        let names = feature_names();
        assert_eq!(names.iter().collect::<std::collections::HashSet<_>>().len(), names.len());
    }

    /// One message of each kind, both ways.
    fn samples() -> (Vec<ClientMsg>, Vec<ServerMsg>) {
        let s = |v: &str| v.to_string();
        let p = |v: &str| PathBuf::from(v);
        let requests = vec![
            Request::Ping,
            Request::ListPanes,
            Request::ListAgents,
            Request::ListWorkspaces,
            Request::OpenWorkspace { name: s("w") },
            Request::NewPane {
                target: Some(s("1")),
                command: vec![s("sh")],
                cwd: Some(p("/tmp")),
                name: Some(s("n")),
                placement: Placement::Right,
                preview: Some(p("/tmp/a.md")),
                focus: true,
                close_on_exit: true,
            },
            Request::ClosePane { pane: s("1") },
            Request::FocusPane { pane: s("1") },
            Request::RenamePane { pane: s("1"), name: s("n") },
            Request::SendText { pane: s("1"), text: s("hi"), production: true },
            Request::SendKeys { pane: s("1"), keys: vec![s("ctrl+c")], production: false },
            Request::ReadPane { pane: s("1"), lines: Some(5) },
            Request::Wait { pane: s("1"), until: vec![s("done")], timeout_ms: Some(10) },
            Request::AgentReport {
                pane: s("1"),
                agent: s("claude"),
                state: Some(AgentState::Working),
                session_id: Some(s("x")),
                message: Some(s("m")),
            },
            Request::Notify { title: s("t"), body: s("b") },
            Request::RunbookPlan {
                path: p("/r.toml"),
                target: Some(s("web")),
                params: vec![(s("A"), s("1"))],
                pane: Some(s("1")),
            },
            Request::RunbookRun { token: s("t") },
            Request::Subscribe { events: vec![s("pane.*")], since: Some(3), boot: Some(4) },
            Request::OpenSsh {
                target: s("web"),
                group: false,
                placement: Placement::Tab,
                near: Some(s("1")),
                command: vec![s("top")],
            },
            Request::OpenAgent {
                agent: s("claude"),
                account: Some(s("a")),
                account_id: None,
                placement: Placement::Floating,
                near: None,
                cwd: Some(p("/")),
                args: vec![s("-c")],
            },
            Request::TunnelAdd { server: s("web"), remote: s("80"), local: Some(8080) },
            Request::TunnelRemove { key: s("8080") },
            Request::TunnelList,
            Request::OpenFiles { left: s("/"), right: s("web:/"), near: None },
            Request::EditRemote { target: s("web:/a"), line: Some(3), near: None, production: true, sudo: true },
            Request::SaveWorkspace { workspace: Some(s("w")), name: Some(s("n")) },
            Request::BufferList,
            Request::BufferShow { buffer: Some(s("0")) },
            Request::BufferSet { name: Some(s("n")), text: s("t") },
            Request::BufferPaste { buffer: None, pane: s("1"), production: false },
            Request::BufferDelete { buffer: Some(s("n")), all: false },
            Request::ReloadConfig,
            Request::KillServer { restart: true },
        ];
        let mut client: Vec<ClientMsg> = requests.into_iter().map(ClientMsg::request).collect();
        client.extend([
            ClientMsg::Attach {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
                workspace: Some(s("w")),
                remote: true,
                pane: Some(s("3")),
                installed: Some(s("0.2.0")),
                features: client_features(),
            },
            ClientMsg::Resize { cols: 80, rows: 24 },
            ClientMsg::Detach,
            ClientMsg::PasteStart,
            ClientMsg::AttachmentRequest { token: s("ab"), text: b"/tmp/a".to_vec(), image: false, pane: None },
            ClientMsg::AttachmentRequest { token: s("cd"), text: Vec::new(), image: true, pane: Some(7) },
            ClientMsg::AttachmentText { token: s("ab"), text: b"/tmp/a".to_vec() },
            ClientMsg::AttachmentStart {
                token: s("ab"),
                files: vec![crate::attachments::FileSpec { name: s("notes.txt"), size: 3 }],
            },
            ClientMsg::AttachmentChunk { token: s("ab"), file: 0, offset: 0, data: s("YWJj") },
            ClientMsg::AttachmentFinish { token: s("ab") },
            ClientMsg::AttachmentCancel { token: s("ab") },
            ClientMsg::AttachmentStatus { token: s("ab") },
            ClientMsg::AttachmentDismiss { token: s("ab") },
            ClientMsg::AttachmentLocal { token: s("ab"), paths: vec![s("/tmp/notes.txt")] },
            ClientMsg::ClipboardResult { id: 1, outcome: ClipboardOutcome::Terminal, native_unavailable: true },
        ]);
        let pane = PaneInfo {
            account: None,
            account_id: None,
            id: 1,
            name: Some(s("n")),
            title: s("t"),
            workspace: s("w"),
            tab: s("t"),
            kind: s("shell"),
            command: vec![s("sh")],
            cwd: Some(p("/")),
            agent: Some(s("claude")),
            state: AgentState::Blocked,
            exit_code: Some(1),
            suspended: true,
            keyboard: 1,
            server: Some(s("web")),
            session: Some(s("main")),
            production: Some(s("server web")),
            contexts: vec![s("tf:prod")],
            focused: true,
            floating: false,
            cols: 80,
            rows: 24,
        };
        let tunnel = crate::server::net::TunnelInfo {
            id: 1,
            spec: crate::server::net::TunnelSpec {
                server: s("web"),
                via: Some(crate::servers::Server { name: s("web"), host: s("10.0.0.1"), ..Default::default() }),
                local: 8080,
                remote_host: s("localhost"),
                remote_port: 80,
            },
            state: crate::server::net::TunnelState::Connecting,
            error: Some(s("e")),
        };
        let answers = vec![
            Response::Ok,
            Response::Pong { pid: 1, session: s("main"), version: s("0.2.0"), features: Some(feature_names()) },
            Response::Pane(Box::new(pane.clone())),
            Response::Panes(vec![pane]),
            Response::Workspaces(vec![WorkspaceInfo {
                id: Some(s("workspace-id")),
                name: s("w"),
                open: true,
                definition: Some(p("/w.kdl")),
                tabs: 1,
                panes: 2,
                working: 1,
                blocked: 0,
                done: 0,
            }]),
            Response::Text(s("t")),
            Response::Plan { text: s("t"), token: s("k") },
            Response::Path(p("/p")),
            Response::Tunnels(vec![tunnel]),
            Response::Buffers(vec![BufferInfo {
                buffer: s("0"),
                named: false,
                bytes: 2,
                lines: 1,
                at: 5,
                preview: s("hi"),
            }]),
        ];
        let mut server: Vec<ServerMsg> = answers.into_iter().map(|a| ServerMsg::Response { result: Ok(a) }).collect();
        server.extend([
            ServerMsg::Response { result: Err(s("no")) },
            ServerMsg::Exit { reason: s("bye"), reattach: Some(s("w")) },
            ServerMsg::Attached { version: s("0.2.0"), features: Some(feature_names()) },
            ServerMsg::Open { url: s("https://example.com") },
            ServerMsg::ClipboardFocus { pane: Some(7), agent: true },
            ServerMsg::ClipboardFocus { pane: None, agent: false },
            ServerMsg::Clipboard { id: 1, origin: ClipboardOrigin::User, text: s("hello") },
            ServerMsg::AttachmentPrepared {
                token: s("ab"),
                target: s("main"),
                limits: Default::default(),
                broadcast: true,
            },
            ServerMsg::ClipboardImageOffer { token: s("ab"), target: s("main"), limits: Default::default() },
            ServerMsg::AttachmentOffer { token: s("ab"), target: s("main"), limits: Default::default() },
            ServerMsg::AttachmentAck { token: s("ab"), received: 3 },
            ServerMsg::AttachmentResult {
                kind: crate::theme::NoticeKind::Notice,
                pasted: true,
                token: s("ab"),
                message: s("Files received."),
            },
            ServerMsg::Event { event: serde_json::json!({"v": 1, "type": "pane.opened"}) },
            ServerMsg::Events { events: vec![serde_json::json!({"v": 1, "type": "pane.closed"})] },
        ]);
        (client, server)
    }

    /// `v` with a field no kraal knows yet in each of its objects, except the free-form events
    /// and the `Ok`/`Err` of a result, which hold one key.
    fn with_new_fields(v: &serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match v {
            Value::Object(map) => {
                let result = map.len() == 1 && (map.contains_key("Ok") || map.contains_key("Err"));
                let mut out: serde_json::Map<String, Value> = map
                    .iter()
                    .map(|(k, v)| {
                        let v = if k == "event" || k == "events" { v.clone() } else { with_new_fields(v) };
                        (k.clone(), v)
                    })
                    .collect();
                if !result {
                    out.insert("zz_new".into(), serde_json::json!({"a": [1, "b", null]}));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(with_new_fields).collect()),
            v => v.clone(),
        }
    }

    /// The JSON pointers to every string in `v`.
    fn strings(v: &serde_json::Value, at: String, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(_) => out.push(at),
            serde_json::Value::Object(map) => map.iter().for_each(|(k, v)| strings(v, format!("{at}/{k}"), out)),
            serde_json::Value::Array(items) => {
                items.iter().enumerate().for_each(|(i, v)| strings(v, format!("{at}/{i}"), out))
            }
            _ => {}
        }
    }

    /// Whatever a newer peer adds (fields, request kinds, message types, answer kinds, enum
    /// values) never fails a message on either side, and new fields change nothing in it.
    #[test]
    fn attachment_result_kinds_are_optional_and_ignored_by_older_readers() {
        use crate::theme::NoticeKind;

        let old = serde_json::json!({"type": "attachment_result", "token": "ab", "message": "Files received."});
        let ServerMsg::AttachmentResult { kind, message, .. } = serde_json::from_value(old).unwrap() else { panic!() };
        assert_eq!(kind, NoticeKind::Notice);
        assert_eq!(message, "Files received.");
        #[derive(Deserialize)]
        struct OlderResult {
            token: String,
            message: String,
            pasted: bool,
        }
        for kind in [NoticeKind::Notice, NoticeKind::Warning, NoticeKind::Error] {
            let msg = ServerMsg::AttachmentResult { kind, token: "ab".into(), message: "result".into(), pasted: true };
            let json = serde_json::to_value(msg).unwrap();
            let older: OlderResult = serde_json::from_value(json.clone()).unwrap();
            assert_eq!((older.token.as_str(), older.message.as_str(), older.pasted), ("ab", "result", true));
            let ServerMsg::AttachmentResult { kind: read, .. } = serde_json::from_value(json).unwrap() else {
                panic!()
            };
            assert_eq!(read, kind);
        }
    }

    #[test]
    fn every_message_reads_whatever_a_newer_peer_adds() {
        // Where the message type, the request and the answer kind are named.
        const TAGS: [&str; 3] = ["/type", "/request/cmd", "/result/Ok/kind"];
        /// `unknown` says whether a message was read as one this kraal does not know.
        fn check<T: Serialize + serde::de::DeserializeOwned>(msg: &T, unknown: fn(&T) -> bool) {
            let v = serde_json::to_value(msg).unwrap();
            let extended = with_new_fields(&v);
            let read: T = serde_json::from_value(extended.clone()).unwrap_or_else(|e| panic!("{extended}: {e}"));
            assert_eq!(serde_json::to_value(&read).unwrap(), v, "new fields changed {extended}");
            let mut pointers = Vec::new();
            strings(&v, String::new(), &mut pointers);
            for at in pointers {
                let mut changed = v.clone();
                *changed.pointer_mut(&at).unwrap() = "zz_new".into();
                let read = serde_json::from_value::<T>(changed.clone())
                    .unwrap_or_else(|e| panic!("a new value at {at} fails {changed}: {e}"));
                // A new value anywhere else is read in the message it belongs to.
                assert_eq!(unknown(&read), TAGS.contains(&at.as_str()), "a new value at {at}: {changed}");
            }
        }
        let (client, server) = samples();
        for msg in &client {
            check(msg, |m| {
                matches!(m, ClientMsg::Unknown(_) | ClientMsg::Request { request: Request::Unknown(_), .. })
            });
        }
        for msg in &server {
            check(msg, |m| {
                matches!(m, ServerMsg::Unknown(_) | ServerMsg::Response { result: Ok(Response::Unknown(_)) })
            });
        }
        // A new kind of answer with data, or a unit answer that got some.
        for answer in [r#"{"kind":"maps","data":[{"x":1}]}"#, r#"{"kind":"ok","data":{"x":1}}"#] {
            let msg = format!(r#"{{"type":"response","result":{{"Ok":{answer}}}}}"#);
            assert!(
                matches!(
                    serde_json::from_str::<ServerMsg>(&msg),
                    Ok(ServerMsg::Response { result: Ok(Response::Unknown(_)) })
                ),
                "{msg}"
            );
        }
    }

    /// Frames and messages as a peer may send them, any of them: read bounded, never a panic.
    #[test]
    fn fuzz_protocol() {
        crate::fuzz::run("protocol", 300, |rng| {
            let mut wire = Vec::new();
            for _ in 0..rng.range(1, 6) {
                let payload: Vec<u8> = match rng.below(4) {
                    0 => serde_json::to_vec(&ClientMsg::Attach {
                        version: PROTOCOL_VERSION,
                        cols: rng.number() as u16,
                        rows: rng.number() as u16,
                        workspace: None,
                        remote: rng.one_in(2),
                        pane: None,
                        installed: None,
                        features: client_features(),
                    })
                    .unwrap(),
                    1 => br#"{"type":"exit","reason":"x"}"#.to_vec(),
                    2 => br#"{"cmd":"kill_server"}"#.to_vec(),
                    _ => {
                        let n = rng.size(64);
                        rng.bytes(n)
                    }
                };
                write_frame(&mut wire, rng.below(3) as u8, &payload).unwrap();
            }
            if rng.one_in(2) {
                rng.mutate(&mut wire, &[&[0xff, 0xff, 0xff, 0x7f], &[0, 0, 0, 0], b"{", b"\""]);
            }
            let mut r = std::io::Cursor::new(wire);
            while let Ok(Some((_, payload))) = read_frame(&mut r) {
                assert!(payload.len() <= MAX_FRAME);
                let _ = serde_json::from_slice::<ClientMsg>(&payload);
                let _ = serde_json::from_slice::<ServerMsg>(&payload);
                let _ = serde_json::from_slice::<Request>(&payload);
            }
            let v = String::from_utf8_lossy(&rng.text(20)).into_owned();
            let _ = newer(&v, KRAAL_VERSION) || newer(KRAAL_VERSION, &v);
        });
    }
}
