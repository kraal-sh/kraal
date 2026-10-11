//! Clients: connections, their frames, attaching and detaching.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::{Client, ClientId, Event, Mode, Server};
use crate::ansi::Encoder;
use crate::input::InputParser;
use crate::pane::PaneSpec;
use crate::protocol::{self, ClientMsg, ServerMsg};
/// Clients connected at once (attached terminals, CLI commands, event streams).
const MAX_CLIENTS: usize = 256;

impl Server {
    pub(super) fn new_connection(&mut self, stream: UnixStream) {
        // Every connection costs two threads: a runaway script must not open thousands.
        if self.clients.len() + self.subscribers.len() + self.waits.len() >= MAX_CLIENTS {
            let mut stream = stream;
            let refused = ServerMsg::Response { result: Err(format!("too many connections ({MAX_CLIENTS})")) };
            let _ = protocol::write_json(&mut stream, &refused);
            return;
        }
        let id = self.alloc_id();
        let Ok(read_half) = stream.try_clone() else { return };
        let (out_tx, out_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);
        let mut write_half = stream;
        // A client that stops reading is let go rather than holding its writer forever.
        let _ = write_half.set_write_timeout(Some(Duration::from_secs(10)));
        // And one that connects without saying anything.
        let _ = read_half.set_read_timeout(Some(Duration::from_secs(10)));
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
            let mut first = true;
            while let Ok(Some((kind, payload))) = protocol::read_frame(&mut r) {
                if std::mem::take(&mut first) {
                    let _ = r.get_ref().set_read_timeout(None);
                }
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
                sidebar: None,
                dirty: false,
                last_input: Instant::now(),
                terminal_focused: true,
                pointer_motion: false,
                keys_full: None,
                keys_by_place: false,
                paste: None,
                drag: None,
                toast: None,
                last_click: None,
                welcome: false,
                remote: false,
            },
        );
    }

    pub(super) fn client_frame(&mut self, id: ClientId, kind: u8, payload: Vec<u8>) {
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
            ClientMsg::Attach { version, cols, rows, workspace, remote } => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.remote = remote;
                }
                let (cols, rows) = clamp_size(cols, rows);
                if version != protocol::PROTOCOL_VERSION {
                    self.send_msg(id, &ServerMsg::Exit { reason: version_mismatch(version) });
                    return;
                }
                self.attach(id, cols, rows, workspace);
            }
            ClientMsg::Resize { cols, rows } => {
                let (cols, rows) = clamp_size(cols, rows);
                if let Some(c) = self.clients.get_mut(&id) {
                    c.cols = cols;
                    c.rows = rows;
                    c.encoder.invalidate();
                }
                self.primary = Some(id);
                self.probe_cell_size(id);
                self.relayout();
            }
            ClientMsg::Detach => self.detach(id, "detached"),
            ClientMsg::Request { version, request } => {
                let result = if version != protocol::PROTOCOL_VERSION {
                    Some(Err(version_mismatch(version)))
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

    pub(super) fn attach(&mut self, id: ClientId, cols: u16, rows: u16, workspace: Option<String>) {
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
                self.drop_empty_workspaces();
                self.send_msg(id, &ServerMsg::Exit { reason: format!("cannot start a shell: {e:#}") });
                return;
            }
        }
        let ws = target.unwrap_or_else(|| self.workspaces[0].id);
        let welcome = !crate::paths::state_dir().join("welcomed").exists();
        if let Some(c) = self.clients.get_mut(&id) {
            c.attached = true;
            c.cols = cols;
            c.rows = rows;
            c.workspace = ws;
            c.dirty = true;
            c.welcome = welcome;
            // Ask the outer terminal for its colours so panes querying them get real values.
            let _ = c.out.try_send(frame_bytes(b"\x1b]10;?\x07\x1b]11;?\x07"));
            // The mouse as the server's configuration says, which may have been read again since
            // the client read its own (a reconnection keeps the client's first reading).
            let mouse = if self.config.mouse { crate::client::MOUSE_ON } else { crate::client::MOUSE_OFF };
            let _ = c.out.try_send(frame_bytes(mouse.as_bytes()));
        }
        self.probe_graphics(id);
        self.primary = Some(id);
        self.relayout();
        self.mark_seen_focused(id);
        self.show_warnings();
        self.mark_dirty();
    }

    pub fn detach(&mut self, id: ClientId, reason: &str) {
        self.graphics_farewell(id);
        self.send_msg(id, &ServerMsg::Exit { reason: reason.to_string() });
        self.drop_client(id);
    }

    pub(super) fn drop_client(&mut self, id: ClientId) {
        self.clients.remove(&id);
        // A `kraal wait` or `kraal events` whose caller went away is dropped with it.
        self.waits.retain(|w| w.client != id);
        self.subscribers.retain(|s| s.client != id);
        // What the client was in the middle of goes with it.
        self.drafts.remove(&id);
        self.md_sessions.remove(&id);
        for p in self.panes.values_mut() {
            if let crate::pane::Content::Files(f) = &mut p.content {
                f.forget_viewer(id);
            }
        }
        if self.primary == Some(id) {
            self.primary = self.clients.values().filter(|c| c.attached).max_by_key(|c| c.last_input).map(|c| c.id);
            self.relayout();
        }
        self.graphics_client_gone(id);
    }

    pub fn send_msg(&self, id: ClientId, msg: &ServerMsg) {
        if let Some(c) = self.clients.get(&id) {
            let mut buf = Vec::new();
            if let Err(e) = protocol::write_json(&mut buf, msg) {
                // Too large to send: the client must not wait for an answer that never comes.
                buf.clear();
                let answer = ServerMsg::Response { result: Err(format!("the answer could not be sent: {e}")) };
                let _ = protocol::write_json(&mut buf, &answer);
            }
            let _ = c.out.send_timeout(buf, Duration::from_millis(200));
        }
    }
}

/// What a client from another kraal version is told. `kraal kill` stops any version's server.
fn version_mismatch(version: u32) -> String {
    format!(
        "protocol mismatch: server v{}, client v{version}; run `kraal kill`, then `kraal` again (the layout is kept)",
        protocol::PROTOCOL_VERSION
    )
}

/// A client's terminal size, within what kraal draws (a bogus size must not allocate gigabytes).
fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, 1000), rows.clamp(1, 500))
}

/// Terminal bytes in a protocol frame.
pub fn frame_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 5);
    let _ = protocol::write_frame(&mut out, protocol::BYTES, bytes);
    out
}
