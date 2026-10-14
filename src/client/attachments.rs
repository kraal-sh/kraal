//! The server can offer a form, but only local keyboard input authorizes opening files.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use crossbeam_channel::{Receiver, Sender, bounded};

use crate::attachments::{self, Source};
use crate::input::{InputEvent, InputParser, KeyCode};
use crate::protocol::{self, ClientMsg, ServerMsg};

type Writer = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

struct Prompt {
    token: String,
    target: String,
    limits: attachments::Settings,
    text: String,
    cursor: usize,
    error: Option<String>,
    paste_failed: bool,
    parser: InputParser,
}

struct Active {
    token: String,
    cancelled: Arc<AtomicBool>,
    ack: Sender<u64>,
    started: Instant,
    progress: Option<(u64, u64)>,
}

#[derive(Default)]
struct State {
    prompt: Option<Prompt>,
    active: Option<Active>,
    status: Option<String>,
    uncertain: Option<String>,
    result_token: Option<String>,
    parser: InputParser,
}

pub(super) struct Controller {
    state: Mutex<State>,
    writer: Writer,
    remote: bool,
}

impl Controller {
    pub fn new(writer: Writer, remote: bool) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::default()), writer, remote })
    }

    pub fn draw(&self) {
        let state = self.state.lock().unwrap();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let width = usize::from(cols).saturating_sub(2).max(1);
        let lines = if let Some(p) = &state.prompt {
            let before: String = p.text.chars().take(p.cursor).collect();
            vec![
                format!("Attach files to {}", p.target),
                format!("Paths: {}", crate::text::truncate_start(&before, width.saturating_sub(7))),
                p.error.clone().unwrap_or_else(|| "Absolute paths, quote spaces. Enter sends, Ctrl+C cancels.".into()),
            ]
        } else if let Some(status) = &state.status {
            let mut lines = crate::text::wrap(status, width);
            lines.truncate(4);
            lines.push("Ctrl+C dismisses or cancels. Use Attach files to send another batch.".into());
            lines
        } else {
            return;
        };
        let count = lines.len().min(usize::from(rows));
        let mut out = String::from("\x1b7\x1b[?7l");
        for (i, line) in lines.iter().take(count).enumerate() {
            let row = usize::from(rows) - count + i + 1;
            out.push_str(&format!("\x1b[{row};1H\x1b[0m {}\x1b[K", crate::text::truncate(line, width)));
        }
        out.push_str("\x1b[?7h\x1b8");
        let _ = super::terminal().write(out.as_bytes());
    }

    fn redraw(&self) {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let _ = send(&self.writer, &ClientMsg::Resize { cols, rows });
    }

    pub fn message(&self, msg: ServerMsg) {
        let mut state = self.state.lock().unwrap();
        match msg {
            ServerMsg::AttachmentOffer { token, target, limits } => {
                if state.prompt.is_some() || state.active.is_some() || !attachments::token_valid(&token) {
                    return;
                }
                state.status = None;
                state.prompt = Some(Prompt {
                    token,
                    target: target.chars().filter(|c| !c.is_control()).collect(),
                    limits,
                    text: String::new(),
                    cursor: 0,
                    error: None,
                    paste_failed: false,
                    parser: InputParser::new(),
                });
            }
            ServerMsg::AttachmentAck { token, received } => {
                if let Some(active) = &state.active
                    && active.token == token
                {
                    let _ = active.ack.try_send(received);
                }
            }
            ServerMsg::AttachmentResult { token, message } => {
                let active = state.active.as_ref().is_some_and(|a| a.token == token);
                let prompt = state.prompt.as_ref().is_some_and(|p| p.token == token);
                let uncertain = state.uncertain.as_ref() == Some(&token);
                if !active && !prompt && !uncertain {
                    return;
                }
                if active && let Some(active) = state.active.take() {
                    active.cancelled.store(true, Ordering::SeqCst);
                }
                if prompt {
                    state.prompt = None;
                }
                if uncertain {
                    state.uncertain = None;
                }
                if state.active.is_none() && state.prompt.is_none() {
                    state.result_token = Some(token);
                    state.status = Some(message.chars().filter(|c| !c.is_control()).collect());
                }
            }
            _ => {}
        }
        drop(state);
        self.draw();
    }

    /// True when the local form consumed the input. Normal typing stays on the same wire.
    pub fn keys(self: &Arc<Self>, bytes: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.prompt.is_none() {
            if (state.active.is_some() || state.status.is_some())
                && state.parser.feed(bytes).iter().any(
                    |event| matches!(event, InputEvent::Key(key) if key.code == KeyCode::Char('c') && key.mods.ctrl),
                )
            {
                drop(state);
                self.cancel();
                return true;
            }
            return false;
        }
        let Some(prompt) = state.prompt.as_mut() else { return false };
        let mut submit = false;
        let mut tail = &bytes[bytes.len()..];
        'input: for (index, byte) in bytes.iter().enumerate() {
            for event in prompt.parser.feed(std::slice::from_ref(byte)) {
                match event {
                    InputEvent::Key(key) if key.code == KeyCode::Enter => {
                        if prompt.error.is_none() {
                            submit = true;
                            tail = &bytes[index + 1..];
                        }
                        break 'input;
                    }
                    InputEvent::Key(key) if key.code == KeyCode::Char('c') && key.mods.ctrl => {
                        drop(state);
                        self.cancel();
                        return true;
                    }
                    InputEvent::Key(key) if matches!(key.code, KeyCode::Char(_)) && !key.mods.ctrl && !key.mods.alt => {
                        let KeyCode::Char(c) = key.code else { unreachable!() };
                        if let Err(error) =
                            crate::server::paste_line(&mut prompt.text, &mut prompt.cursor, &c.to_string())
                        {
                            prompt.error = Some(error);
                        }
                    }
                    InputEvent::Key(key) => {
                        let before = prompt.text.clone();
                        let edited = crate::server::edit_line(&mut prompt.text, &mut prompt.cursor, &key);
                        if edited && (prompt.text != before || (key.code == KeyCode::Char('u') && key.mods.ctrl)) {
                            prompt.error = None;
                        }
                    }
                    event @ (InputEvent::Paste(_) | InputEvent::PastePiece(_)) => {
                        let last = matches!(event, InputEvent::Paste(_));
                        let (InputEvent::Paste(data) | InputEvent::PastePiece(data)) = event else { unreachable!() };
                        if !prompt.paste_failed {
                            let text = String::from_utf8_lossy(&data);
                            let error = if text.chars().any(char::is_control) {
                                Some("Enter paths on one line, quoting spaces. Ctrl+U clears the field.".into())
                            } else {
                                crate::server::paste_line(&mut prompt.text, &mut prompt.cursor, &text).err()
                            };
                            if error.is_some() {
                                prompt.error = error;
                            }
                            prompt.paste_failed = prompt.error.is_some();
                        }
                        if last {
                            prompt.paste_failed = false;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !submit {
            drop(state);
            self.draw();
            return true;
        }
        let prompt = state.prompt.take().unwrap();
        let (ack, replies) = bounded(2);
        let cancelled = Arc::new(AtomicBool::new(false));
        state.active = Some(Active {
            token: prompt.token.clone(),
            cancelled: cancelled.clone(),
            ack,
            started: Instant::now(),
            progress: None,
        });
        state.status = None;
        state.parser = InputParser::new();
        drop(state);
        self.start_progress(prompt.token.clone());
        let this = self.clone();
        let token = prompt.token.clone();
        if let Err(error) = crate::threads::named("attachments").spawn(move || {
            if let Err(error) = this.upload(&prompt, &replies, &cancelled)
                && !cancelled.load(Ordering::SeqCst)
            {
                {
                    let mut writer = this.writer.lock().unwrap();
                    if !cancelled.load(Ordering::SeqCst)
                        && let Some(writer) = writer.as_mut()
                    {
                        let _ =
                            protocol::write_json(writer, &ClientMsg::AttachmentCancel { token: prompt.token.clone() });
                    }
                }
                this.message(ServerMsg::AttachmentResult {
                    token: prompt.token,
                    message: format!("Attachment stopped: {error:#}. Use Attach files to retry."),
                });
            }
        }) {
            let _ = send(&self.writer, &ClientMsg::AttachmentCancel { token: token.clone() });
            self.message(ServerMsg::AttachmentResult {
                token,
                message: format!("Cannot start attachment: {error}. Use Attach files to retry."),
            });
        }
        if !tail.is_empty() && !self.keys(tail) {
            let mut writer = self.writer.lock().unwrap();
            if let Some(writer) = writer.as_mut() {
                let _ = protocol::write_frame(writer, protocol::BYTES, tail);
            }
        }
        self.redraw();
        self.draw();
        true
    }

    fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        let token = state.prompt.take().map(|p| p.token).or_else(|| {
            state.active.take().map(|a| {
                a.cancelled.store(true, Ordering::SeqCst);
                a.token
            })
        });
        let dismiss = state.result_token.take().or_else(|| state.uncertain.take());
        state.status = token.as_ref().map(|_| "Cancelling attachment. Waiting for its result.".into());
        state.uncertain = token.clone();
        drop(state);
        if let Some(token) = token {
            let _ = send(&self.writer, &ClientMsg::AttachmentCancel { token });
        } else if let Some(token) = dismiss {
            let _ = send(&self.writer, &ClientMsg::AttachmentDismiss { token });
        }
        self.redraw();
    }

    fn progress(&self, token: &str, received: u64, total: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(active) = &mut state.active
            && active.token == token
        {
            active.progress = Some((received, total));
        }
    }

    fn start_progress(self: &Arc<Self>, token: String) {
        let this = Arc::downgrade(self);
        let _ = crate::threads::named("attachment-progress").spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let Some(this) = this.upgrade() else { return };
                let mut state = this.state.lock().unwrap();
                let Some(active) = state.active.as_ref().filter(|active| active.token == token) else { return };
                if active.started.elapsed() < std::time::Duration::from_millis(200) {
                    continue;
                }
                let progress = active.progress.map_or_else(
                    || "Preparing files".into(),
                    |(received, total)| format!("Sending: {received}/{total} bytes"),
                );
                state.status = Some(format!("{progress}. Typing waits. Ctrl+C cancels."));
                drop(state);
                this.draw();
            }
        });
    }

    fn upload(&self, prompt: &Prompt, replies: &Receiver<u64>, cancelled: &AtomicBool) -> Result<()> {
        let started = Instant::now();
        let paths = explicit_paths(&prompt.text)?;
        ensure!(paths.len() <= prompt.limits.files, "too many files; use kraal cp");
        let mut sources = paths.into_iter().map(Source::open).collect::<Result<Vec<_>>>()?;
        let specs: Vec<_> = sources.iter().map(|s| s.spec.clone()).collect();
        let token = &prompt.token;
        let write = |msg: ClientMsg| -> Result<()> {
            let mut writer = self.writer.lock().unwrap();
            ensure!(!cancelled.load(Ordering::SeqCst), "attachment cancelled");
            ensure!(started.elapsed() < attachments::TIMEOUT, "attachment timed out");
            protocol::write_json(writer.as_mut().context("connection closed")?, &msg)?;
            Ok(())
        };
        let wait = |expected| -> Result<()> {
            loop {
                ensure!(!cancelled.load(Ordering::SeqCst), "attachment cancelled");
                ensure!(started.elapsed() < attachments::TIMEOUT, "attachment timed out");
                match replies.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(received) => {
                        ensure!(received == expected, "unexpected attachment acknowledgement");
                        return Ok(());
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(_) => bail!("connection closed"),
                }
            }
        };
        if !self.remote {
            let paths = sources
                .iter()
                .map(|s| s.path.to_str().context("path is not UTF-8").map(String::from))
                .collect::<Result<_>>()?;
            write(ClientMsg::AttachmentLocal { token: token.clone(), paths })?;
            return Ok(());
        }
        let total = prompt.limits.check(&specs)?;
        write(ClientMsg::AttachmentStart { token: token.clone(), files: specs })?;
        self.progress(token, 0, total);
        wait(0)?;
        let mut received = 0;
        for (file, source) in sources.iter_mut().enumerate() {
            let mut offset = 0;
            while offset < source.spec.size {
                ensure!(!cancelled.load(Ordering::SeqCst), "attachment cancelled");
                let bytes = source.chunk(offset)?;
                let size = bytes.len() as u64;
                write(ClientMsg::AttachmentChunk {
                    token: token.clone(),
                    file,
                    offset,
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                })?;
                offset += size;
                received += size;
                wait(received)?;
                self.progress(token, received, total);
            }
            source.chunk(offset)?;
        }
        write(ClientMsg::AttachmentFinish { token: token.clone() })?;
        Ok(())
    }

    pub fn disconnect(&self) {
        let mut state = self.state.lock().unwrap();
        state.prompt = None;
        if let Some(active) = state.active.take() {
            active.cancelled.store(true, Ordering::SeqCst);
            state.uncertain = Some(active.token);
        }
        state.status = None;
        state.result_token = None;
    }

    pub fn reconnect(&self) {
        let token = self.state.lock().unwrap().uncertain.clone();
        if let Some(token) = token {
            let _ = send(&self.writer, &ClientMsg::AttachmentStatus { token });
        }
    }
}

fn send(writer: &Writer, msg: &ClientMsg) -> Result<()> {
    let mut writer = writer.lock().unwrap();
    protocol::write_json(writer.as_mut().context("connection closed")?, msg)?;
    Ok(())
}

fn explicit_paths(text: &str) -> Result<Vec<std::path::PathBuf>> {
    ensure!(
        !text.contains("file://") && !text.ends_with('\\') && !text.chars().any(char::is_control),
        "enter absolute paths, quoting spaces; URI paths are not supported here"
    );
    crate::transfer::dropped_paths(text).context("enter existing absolute paths, quoting spaces")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Duration;

    struct Wire(Sender<Vec<u8>>);
    impl Write for Wire {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.send(bytes.to_vec()).map_err(std::io::Error::other)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    struct Reader {
        incoming: Receiver<Vec<u8>>,
        bytes: std::io::Cursor<Vec<u8>>,
    }
    impl Read for Reader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.bytes.position() == self.bytes.get_ref().len() as u64 {
                self.bytes = std::io::Cursor::new(
                    self.incoming.recv_timeout(Duration::from_secs(2)).map_err(std::io::Error::other)?,
                );
            }
            self.bytes.read(out)
        }
    }
    fn wire() -> (Box<dyn Write + Send>, Reader) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (Box::new(Wire(tx)), Reader { incoming: rx, bytes: std::io::Cursor::new(Vec::new()) })
    }
    fn controller(remote: bool) -> (Arc<Controller>, Reader) {
        let (writer, peer) = wire();
        (Controller::new(Arc::new(Mutex::new(Some(writer))), remote), peer)
    }
    fn read(peer: &mut Reader) -> ClientMsg {
        let (kind, data) = protocol::read_frame(peer).unwrap().unwrap();
        assert_eq!(kind, protocol::JSON);
        serde_json::from_slice(&data).unwrap()
    }

    #[test]
    fn cancellation_parses_encoded_fragmented_and_coalesced_keys_in_every_state() {
        for input in [b"\x03".as_slice(), b"\x1b[99;5u", b"before\x03after", b"before\x1b[99;5uafter"] {
            for fragmented in [false, true] {
                for phase in ["prompt", "active", "result"] {
                    let (client, mut peer) = controller(true);
                    let cancelled = Arc::new(AtomicBool::new(false));
                    if phase == "prompt" {
                        client.message(ServerMsg::AttachmentOffer {
                            token: "ab".into(),
                            target: "pane".into(),
                            limits: Default::default(),
                        });
                    } else {
                        let mut state = client.state.lock().unwrap();
                        if phase == "active" {
                            state.active = Some(Active {
                                token: "ab".into(),
                                cancelled: cancelled.clone(),
                                ack: bounded(2).0,
                                started: Instant::now(),
                                progress: None,
                            });
                        } else {
                            state.status = Some("Failed".into());
                            state.result_token = Some("ab".into());
                        }
                    }
                    if fragmented {
                        for byte in input {
                            client.keys(std::slice::from_ref(byte));
                        }
                    } else {
                        assert!(client.keys(input));
                    }
                    let message = read(&mut peer);
                    if phase == "result" {
                        assert!(matches!(message, ClientMsg::AttachmentDismiss { token } if token == "ab"));
                        assert!(client.state.lock().unwrap().status.is_none());
                    } else {
                        assert!(matches!(message, ClientMsg::AttachmentCancel { token } if token == "ab"));
                        assert!(client.state.lock().unwrap().active.is_none());
                        assert!(client.state.lock().unwrap().prompt.is_none());
                        if phase == "active" {
                            assert!(cancelled.load(Ordering::SeqCst));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn unbracketed_overflow_cannot_submit_an_independently_valid_prefix() {
        let (client, peer) = controller(true);
        let path = std::env::temp_dir().join(format!("kraal-attachment-overflow-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        client.message(ServerMsg::AttachmentOffer {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
        });
        let prefix = format!("{}{}", path.display(), " ".repeat(4096 - path.to_str().unwrap().len()));
        client.keys(format!("{prefix}{}", path.display()).as_bytes());
        {
            let state = client.state.lock().unwrap();
            let prompt = state.prompt.as_ref().unwrap();
            assert_eq!(prompt.text, prefix);
            assert_eq!(explicit_paths(&prompt.text).unwrap(), vec![path.clone()]);
            assert!(prompt.error.is_some());
        }
        client.keys(b"\x1b[D\x1b[C\x1b[200~\x1b[201~\r");
        assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().error.is_some());
        assert!(client.state.lock().unwrap().active.is_none());
        assert!(peer.incoming.try_recv().is_err());
        client.keys(b"\x15");
        assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().error.is_none());
        assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().text.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn progress_and_cancellation_are_visible_without_the_first_acknowledgement() {
        for acknowledge_start in [false, true] {
            let (client, mut peer) = controller(true);
            let path = std::env::temp_dir().join(format!("kraal-attachment-progress-{}", std::process::id()));
            std::fs::write(&path, b"abc").unwrap();
            client.message(ServerMsg::AttachmentOffer {
                token: "ab".into(),
                target: "pane".into(),
                limits: Default::default(),
            });
            client.keys(format!("{}\r", path.display()).as_bytes());
            while !matches!(read(&mut peer), ClientMsg::AttachmentStart { .. }) {}
            if acknowledge_start {
                client.message(ServerMsg::AttachmentAck { token: "ab".into(), received: 0 });
                while !matches!(read(&mut peer), ClientMsg::AttachmentChunk { .. }) {}
            }
            let started = Instant::now();
            loop {
                if let Some(status) = &client.state.lock().unwrap().status {
                    assert!(status.contains("0/3 bytes"));
                    assert!(status.contains("Ctrl+C cancels"));
                    break;
                }
                assert!(started.elapsed() < Duration::from_secs(2));
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(client.keys(b"\x1b[99;5u"));
            client.disconnect();
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_server_offer_never_opens_or_sends_a_file_without_local_confirmation() {
        let (client, mut peer) = controller(true);
        client.state.lock().unwrap().prompt = Some(Prompt {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
            text: "/must/not/be/opened".into(),
            cursor: 0,
            error: None,
            paste_failed: false,
            parser: InputParser::new(),
        });
        assert!(peer.incoming.try_recv().is_err());
        assert!(client.keys(b"\x03"));
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { token } if token == "ab"));
        assert!(!client.keys(b"normal input"));
        assert!(explicit_paths("file:///tmp/nope").is_err());
        assert!(explicit_paths("/tmp/nope\\").is_err());
    }

    #[test]
    fn a_single_chunk_window_waits_for_each_ack_and_keyboard_can_share_the_writer() {
        let (client, mut peer) = controller(true);
        let path = std::env::temp_dir().join(format!("kraal-attachment-window-{}", std::process::id()));
        std::fs::write(&path, vec![b'x'; attachments::CHUNK + 7]).unwrap();
        let prompt = Prompt {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
            text: path.to_str().unwrap().into(),
            cursor: 0,
            error: None,
            paste_failed: false,
            parser: InputParser::new(),
        };
        let (acks, replies) = bounded(2);
        let worker = client.clone();
        let handle = std::thread::spawn(move || worker.upload(&prompt, &replies, &AtomicBool::new(false)));
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentStart { .. }));
        // No ack means no file bytes. The keyboard still obtains the writer immediately.
        send(&client.writer, &ClientMsg::Resize { cols: 90, rows: 30 }).unwrap();
        assert!(matches!(read(&mut peer), ClientMsg::Resize { cols: 90, .. }));
        acks.send(0).unwrap();
        let ClientMsg::AttachmentChunk { offset, data, .. } = read(&mut peer) else { panic!() };
        assert_eq!(offset, 0);
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap().len(), attachments::CHUNK);
        send(&client.writer, &ClientMsg::Resize { cols: 91, rows: 30 }).unwrap();
        assert!(matches!(read(&mut peer), ClientMsg::Resize { cols: 91, .. }));
        acks.send(attachments::CHUNK as u64).unwrap();
        let ClientMsg::AttachmentChunk { offset, data, .. } = read(&mut peer) else { panic!() };
        assert_eq!(offset, attachments::CHUNK as u64);
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), b"xxxxxxx");
        acks.send(attachments::CHUNK as u64 + 7).unwrap();
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentFinish { .. }));
        handle.join().unwrap().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancelled_upload_cannot_write_to_a_replacement_connection() {
        let (client, mut peer) = controller(true);
        let path = std::env::temp_dir().join(format!("kraal-attachment-cancel-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let prompt = Prompt {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
            text: path.to_str().unwrap().into(),
            cursor: 0,
            error: None,
            paste_failed: false,
            parser: InputParser::new(),
        };
        let (acks, replies) = bounded(2);
        let cancelled = Arc::new(AtomicBool::new(false));
        client.state.lock().unwrap().active = Some(Active {
            token: "ab".into(),
            cancelled: cancelled.clone(),
            ack: acks,
            started: Instant::now(),
            progress: None,
        });
        let worker = client.clone();
        let handle = std::thread::spawn(move || worker.upload(&prompt, &replies, &cancelled));
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentStart { .. }));
        client.disconnect();
        let (replacement, mut new_peer) = wire();
        *client.writer.lock().unwrap() = Some(replacement);
        assert!(handle.join().unwrap().is_err());
        assert!(new_peer.incoming.try_recv().is_err());
        client.reconnect();
        assert!(matches!(read(&mut new_peer), ClientMsg::AttachmentStatus { token } if token == "ab"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn local_upload_submits_original_paths_without_file_chunks() {
        let (client, mut peer) = controller(false);
        let path = std::env::temp_dir().join(format!("kraal-attachment-local-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let prompt = Prompt {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
            text: path.to_str().unwrap().into(),
            cursor: 0,
            error: None,
            paste_failed: false,
            parser: InputParser::new(),
        };
        let (_acks, replies) = bounded(2);
        client.upload(&prompt, &replies, &AtomicBool::new(false)).unwrap();
        assert!(
            matches!(read(&mut peer), ClientMsg::AttachmentLocal { paths, .. } if paths == vec![path.to_str().unwrap()])
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn the_form_uses_shared_editing_and_keeps_typing_after_enter_in_order() {
        let (client, mut peer) = controller(true);
        let path = std::env::temp_dir().join(format!("kraal-attachment-form-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        client.state.lock().unwrap().prompt = Some(Prompt {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
            text: String::new(),
            cursor: 0,
            error: None,
            paste_failed: false,
            parser: InputParser::new(),
        });
        assert!(client.keys(b"wrong"));
        assert!(client.keys(b"\x15"));
        assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().text.is_empty());
        assert!(client.keys(b"\x1b[200~/bad\npath\x1b[201~"));
        assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().error.is_some());
        assert!(client.keys(b"\r"));
        assert!(client.state.lock().unwrap().active.is_none());
        client.keys(b"\x15");
        client.keys(b"\x1b[200~");
        client.keys(&vec![b'x'; 128 * 1024]);
        client.keys(format!(" {}\x1b[201~\r", path.display()).as_bytes());
        assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().error.is_some());
        assert!(client.state.lock().unwrap().active.is_none());
        client.keys(b"\x15");
        client.keys(format!("{}\rlater", path.display()).as_bytes());
        let mut started = false;
        let mut typed = false;
        for _ in 0..3 {
            let (kind, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
            if kind == protocol::BYTES {
                assert_eq!(bytes, b"later");
                typed = true;
            } else if matches!(serde_json::from_slice::<ClientMsg>(&bytes).unwrap(), ClientMsg::AttachmentStart { .. })
            {
                started = true;
            }
        }
        assert!(started && typed);
        client.disconnect();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn a_recovered_result_cannot_cancel_a_new_batch() {
        let (client, _peer) = controller(true);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (ack, _replies) = bounded(2);
        {
            let mut state = client.state.lock().unwrap();
            state.uncertain = Some("ab".into());
            state.active = Some(Active {
                token: "cd".into(),
                cancelled: cancelled.clone(),
                ack,
                started: Instant::now(),
                progress: None,
            });
        }
        client.message(ServerMsg::AttachmentResult { token: "ab".into(), message: "Files received.".into() });
        let state = client.state.lock().unwrap();
        assert_eq!(state.active.as_ref().unwrap().token, "cd");
        assert!(!cancelled.load(Ordering::SeqCst));
        assert!(state.uncertain.is_none());
        assert!(state.status.is_none());
    }
}
