//! The server can offer a form, but only local keyboard input authorizes opening files.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use crossbeam_channel::{Receiver, Sender, bounded};

use super::paste;
use crate::attachments::{self, Source};
use crate::input::{InputEvent, InputParser, KeyCode};
use crate::protocol::{self, ClientMsg, ServerMsg};

type Writer = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

struct Prompt {
    image: bool,
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
    failure: Option<String>,
}

struct Dropped {
    token: String,
    original: Vec<u8>,
    paths: Vec<std::path::PathBuf>,
    target: String,
    limits: attachments::Settings,
    broadcast: bool,
    ready: bool,
    result: bool,
    retry: bool,
    selected: usize,
    error: Option<String>,
}

#[derive(Default)]
struct State {
    dropped: Option<Dropped>,
    negotiated: bool,
    splitter: paste::Splitter,
    prompt: Option<Prompt>,
    active: Option<Active>,
    workers: Vec<Receiver<()>>,
    status: Option<String>,
    uncertain: Option<String>,
    result_token: Option<String>,
    parser: InputParser,
}

pub(crate) struct Controller {
    state: Mutex<State>,
    writer: Writer,
    remote: bool,
    destination: Option<(Vec<String>, String)>,
}

impl Controller {
    #[cfg(test)]
    pub fn new(writer: Writer, remote: bool) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::default()), writer, remote, destination: None })
    }

    pub fn for_target(writer: Writer, target: &super::Target) -> Arc<Self> {
        let destination = match target {
            super::Target::Remote { ssh, label, .. } => Some((ssh.clone(), label.clone())),
            super::Target::Local { .. } => None,
        };
        Arc::new(Self { state: Mutex::new(State::default()), writer, remote: destination.is_some(), destination })
    }

    pub fn negotiate(&self, enabled: bool) {
        self.state.lock().unwrap().negotiated = enabled;
    }

    pub fn pending_keys(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.splitter.has_pending() || state.dropped.is_some() && state.parser.has_pending()
    }

    pub fn input(self: &Arc<Self>, bytes: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap();
        if bytes.is_empty() && (!self.remote || !state.negotiated) {
            return true;
        }
        if !state.negotiated && state.dropped.is_some() {
            return false;
        }
        if !self.remote || !state.negotiated {
            drop(state);
            return self.keys(bytes);
        }
        let parts =
            if bytes.is_empty() { vec![paste::Part::Bytes(state.splitter.idle())] } else { state.splitter.feed(bytes) };
        drop(state);
        let mut following_paste = false;
        for part in parts {
            match part {
                paste::Part::Start => {
                    let mut state = self.state.lock().unwrap();
                    if state.prompt.is_none()
                        && state.dropped.is_none()
                        && state.active.is_none()
                        && state.uncertain.is_none()
                    {
                        if let Some(token) = state.result_token.take() {
                            let _ = send(&self.writer, &ClientMsg::AttachmentDismiss { token });
                        }
                        let _ = send(&self.writer, &ClientMsg::PasteStart);
                    }
                }
                paste::Part::Bytes(bytes) => {
                    if following_paste {
                        self.preparing_keys(&bytes);
                    } else if !bytes.is_empty() && !self.keys(&bytes) {
                        self.forward(&bytes);
                    }
                }
                paste::Part::Paste(text) => {
                    let paths = std::str::from_utf8(&text).ok().and_then(crate::transfer::dropped_paths);
                    let mut state = self.state.lock().unwrap();
                    if state.prompt.is_some() {
                        drop(state);
                        self.keys(&paste::bracket(&text));
                        continue;
                    }
                    if state.dropped.is_none()
                        && state.active.is_none()
                        && state.uncertain.is_none()
                        && let Some(paths) = paths
                    {
                        let token = format!(
                            "{:x}-{:x}",
                            std::process::id(),
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos()
                        );
                        state.dropped = Some(Dropped {
                            token: token.clone(),
                            original: text.clone(),
                            paths,
                            target: String::new(),
                            limits: Default::default(),
                            broadcast: false,
                            ready: false,
                            result: false,
                            retry: false,
                            selected: 0,
                            error: None,
                        });
                        state.status = None;
                        let dismiss = state.result_token.take();
                        state.parser = InputParser::new();
                        drop(state);
                        if let Some(token) = dismiss {
                            let _ = send(&self.writer, &ClientMsg::AttachmentDismiss { token });
                        }
                        let _ = send(&self.writer, &ClientMsg::AttachmentRequest { token, text });
                        following_paste = true;
                    } else {
                        let blocked = state.uncertain.is_none()
                            && state.dropped.as_ref().is_some_and(|d| d.ready || d.result)
                            && state.active.is_none();
                        drop(state);
                        if following_paste || !blocked {
                            self.forward(&paste::bracket(&text));
                        }
                    }
                }
            }
        }
        if bytes.is_empty() {
            self.keys(&[]);
        }
        true
    }

    // Preparation queues typing on the server, but cancellation stays local. A tail in
    // the same read as a drop cannot select a choice, even if the reply has arrived.
    fn preparing_keys(self: &Arc<Self>, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        if bytes.is_empty() {
            state.parser.flush();
            return;
        }
        for (index, byte) in bytes.iter().enumerate() {
            let cancel =
                state.parser.feed(std::slice::from_ref(byte)).iter().any(
                    |event| matches!(event, InputEvent::Key(key) if key.code == KeyCode::Char('c') && key.mods.ctrl),
                );
            if cancel && (state.dropped.is_some() || state.active.is_some()) {
                let active = state.active.is_some();
                drop(state);
                if active {
                    self.cancel();
                } else {
                    self.drop_action(3);
                }
                let tail = &bytes[index + 1..];
                if !tail.is_empty() && !self.keys(tail) {
                    self.forward(tail);
                }
                return;
            }
        }
        drop(state);
        if !bytes.is_empty() {
            self.forward(bytes);
        }
    }

    fn forward(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        if let Some(writer) = writer.as_mut() {
            let _ = protocol::write_frame(writer, protocol::BYTES, bytes);
        }
    }

    pub fn draw(&self) {
        let state = self.state.lock().unwrap();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let width = usize::from(cols).saturating_sub(2).max(1);
        let lines = if let Some(dropped) = &state.dropped {
            dropped.lines(
                width,
                self.destination.as_ref().map_or("server", |(_, label)| label),
                state.active.is_some(),
                state.status.as_deref(),
            )
        } else if let Some(p) = &state.prompt {
            let before: String = p.text.chars().take(p.cursor).collect();
            if p.image {
                vec![
                    format!("Attach clipboard image to {}", p.target),
                    "Enter reads the local clipboard and attaches its image. Ctrl+C cancels.".into(),
                ]
            } else {
                vec![
                    format!("Attach files to {}", p.target),
                    format!("Paths: {}", crate::text::truncate_start(&before, width.saturating_sub(7))),
                    p.error
                        .clone()
                        .unwrap_or_else(|| "Absolute paths, quote spaces. Enter sends, Ctrl+C cancels.".into()),
                ]
            }
        } else if let Some(status) = &state.status {
            let mut lines = crate::text::wrap(status, width);
            lines.truncate(4);
            lines.push("Ctrl+C dismisses or cancels. Use the command palette to attach again.".into());
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

    pub fn message(self: &Arc<Self>, msg: ServerMsg) {
        let mut state = self.state.lock().unwrap();
        let repaint = state.dropped.is_some() && matches!(&msg, ServerMsg::AttachmentResult { .. });
        let image = matches!(&msg, ServerMsg::ClipboardImageOffer { .. });
        match msg {
            ServerMsg::AttachmentPrepared { token, target, limits, broadcast } => {
                if state.uncertain.as_ref() == Some(&token) {
                    return;
                }
                let Some(dropped) = state.dropped.as_mut().filter(|d| d.token == token && !d.ready && !d.result) else {
                    return;
                };
                dropped.target = target.chars().filter(|c| !c.is_control()).collect();
                dropped.limits = limits;
                dropped.broadcast = broadcast;
                dropped.ready = true;
                dropped.error = dropped.validate().err().map(|e| format!("{e:#}"));
                let automatic = !dropped.retry
                    && dropped.selected == 0
                    && dropped.error.is_none()
                    && !broadcast
                    && self.destination.as_ref().is_some_and(|(ssh, _)| paste::allowed(&paste::permission_path(), ssh));
                drop(state);
                if automatic {
                    self.drop_action(0);
                } else {
                    self.draw();
                }
                return;
            }
            ServerMsg::AttachmentOffer { token, target, limits }
            | ServerMsg::ClipboardImageOffer { token, target, limits } => {
                if state.dropped.is_some()
                    || state.prompt.is_some()
                    || state.active.is_some()
                    || !attachments::token_valid(&token)
                {
                    return;
                }
                state.status = None;
                let mut parser = InputParser::new();
                if state.splitter.streaming() {
                    // The splitter already forwarded the start of this oversized paste.
                    parser.feed(b"\x1b[200~");
                }
                state.prompt = Some(Prompt {
                    image,
                    token,
                    target: target.chars().filter(|c| !c.is_control()).collect(),
                    limits,
                    text: String::new(),
                    cursor: 0,
                    error: None,
                    paste_failed: false,
                    parser,
                });
            }
            ServerMsg::AttachmentAck { token, received } => {
                if let Some(active) = &state.active
                    && active.token == token
                {
                    let _ = active.ack.try_send(received);
                }
            }
            ServerMsg::AttachmentResult { token, mut message, pasted } => {
                let active = state.active.as_ref().is_some_and(|a| a.token == token);
                let prompt = state.prompt.as_ref().is_some_and(|p| p.token == token);
                let uncertain = state.uncertain.as_ref() == Some(&token);
                let dropped = state.dropped.as_ref().is_some_and(|d| d.token == token);
                if !active && !prompt && !uncertain && !dropped {
                    return;
                }
                if active && let Some(active) = state.active.take() {
                    active.cancelled.store(true, Ordering::SeqCst);
                    if let Some(failure) = active.failure {
                        message = failure;
                    }
                }
                if prompt {
                    state.prompt = None;
                }
                if uncertain {
                    state.uncertain = None;
                }
                if pasted && dropped {
                    let drop = state.dropped.take().unwrap();
                    if active
                        && let Some(tail) =
                            message.strip_prefix("Files received. Paths submitted to the reserved pane.")
                    {
                        let noun = if drop.paths.len() == 1 { "file" } else { "files" };
                        message =
                            format!("{} {noun} sent. Paths submitted to {}.{tail}", drop.paths.len(), drop.target);
                    }
                } else if dropped {
                    let drop = state.dropped.as_mut().unwrap();
                    if let Some(error) = &drop.error {
                        message = format!("{error}. {message}");
                    }
                    drop.result = true;
                    drop.selected = 2;
                }
                if state.active.is_none() && state.prompt.is_none() {
                    state.result_token = Some(token);
                    state.status = (!message.is_empty()).then(|| message.chars().filter(|c| !c.is_control()).collect());
                }
            }
            _ => {}
        }
        drop(state);
        if repaint {
            self.redraw();
        }
        self.draw();
    }

    /// True when the local form consumed the input. Normal typing stays on the same wire.
    pub fn keys(self: &Arc<Self>, bytes: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.dropped.is_some() && state.active.is_none() && state.uncertain.is_none() {
            if state.dropped.as_ref().is_some_and(|d| !d.ready && !d.result) {
                drop(state);
                self.preparing_keys(bytes);
                return true;
            }
            let mut action = None;
            let mut tail = &bytes[bytes.len()..];
            'input: for index in 0..bytes.len().max(1) {
                let events =
                    if bytes.is_empty() { state.parser.flush() } else { state.parser.feed(&bytes[index..=index]) };
                for event in events {
                    if let InputEvent::Mouse(mouse) = event {
                        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                        if mouse.row + 1 == rows
                            && mouse.kind == crate::input::MouseKind::Press(crate::input::MouseButton::Left)
                        {
                            let dropped = state.dropped.as_ref().unwrap();
                            let lines = dropped.lines(
                                usize::from(cols).saturating_sub(2).max(1),
                                self.destination.as_ref().map_or("server", |(_, label)| label),
                                false,
                                state.status.as_deref(),
                            );
                            if let Some(line) = lines.last() {
                                action = button_at(line, usize::from(mouse.col));
                            }
                        }
                        if action.is_some() {
                            tail = &bytes[(index + 1).min(bytes.len())..];
                            break 'input;
                        }
                        continue;
                    }
                    let InputEvent::Key(key) = event else { continue };
                    let dropped = state.dropped.as_mut().unwrap();
                    let count = if dropped.result { 3 } else { 4 };
                    match key.code {
                        KeyCode::Char('c') if key.mods.ctrl => action = Some(if dropped.result { 2 } else { 3 }),
                        KeyCode::Esc => action = Some(if dropped.result { 2 } else { 3 }),
                        KeyCode::Tab | KeyCode::Right => dropped.selected = (dropped.selected + 1) % count,
                        KeyCode::BackTab | KeyCode::Left => dropped.selected = (dropped.selected + count - 1) % count,
                        KeyCode::Enter if dropped.ready || dropped.result => action = Some(dropped.selected),
                        _ => {}
                    }
                    if action.is_some() {
                        tail = &bytes[(index + 1).min(bytes.len())..];
                        break 'input;
                    }
                }
            }
            drop(state);
            if let Some(action) = action {
                self.drop_action(action);
                if !tail.is_empty() && !self.keys(tail) {
                    self.forward(tail);
                }
            } else {
                self.draw();
            }
            return true;
        }
        if state.prompt.is_none() {
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            let cancel_column = state.dropped.as_ref().and_then(|dropped| {
                dropped
                    .lines(
                        usize::from(cols).saturating_sub(2).max(1),
                        self.destination.as_ref().map_or("server", |(_, label)| label),
                        true,
                        state.status.as_deref(),
                    )
                    .last()
                    .and_then(|line| {
                        line.rfind('[').map(|at| {
                            (
                                unicode_width::UnicodeWidthStr::width(&line[..at]) + 1,
                                unicode_width::UnicodeWidthStr::width(line.as_str()) + 1,
                            )
                        })
                    })
            });
            if (state.active.is_some() || state.status.is_some())
                && state.parser.feed(bytes).iter().any(|event| match event {
                    InputEvent::Key(key) => key.code == KeyCode::Char('c') && key.mods.ctrl,
                    InputEvent::Mouse(mouse) => {
                        mouse.kind == crate::input::MouseKind::Press(crate::input::MouseButton::Left)
                            && mouse.row + 1 == rows
                            && cancel_column.is_some_and(|(start, end)| (start..end).contains(&usize::from(mouse.col)))
                    }
                    _ => false,
                })
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
                    _ if prompt.image => {}
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
        drop(state);
        self.start(prompt);
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

    fn start(self: &Arc<Self>, prompt: Prompt) {
        let mut state = self.state.lock().unwrap();
        let (ack, replies) = bounded(2);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (finished, completion) = bounded(1);
        state.workers.retain(|worker| matches!(worker.try_recv(), Err(crossbeam_channel::TryRecvError::Empty)));
        state.workers.push(completion);
        state.active = Some(Active {
            token: prompt.token.clone(),
            cancelled: cancelled.clone(),
            ack,
            started: Instant::now(),
            progress: None,
            failure: None,
        });
        state.status = None;
        state.parser = InputParser::new();
        drop(state);
        self.start_progress(prompt.token.clone());
        let this = self.clone();
        let token = prompt.token.clone();
        let action = if prompt.image { "Attach clipboard image" } else { "Attach files" };
        if let Err(error) = crate::threads::named("attachments").spawn(move || {
            let _finished = finished;
            if let Err(error) = this.upload(&prompt, &replies, &cancelled)
                && !cancelled.load(Ordering::SeqCst)
            {
                this.upload_failed(prompt.token, &cancelled, &error, prompt.image);
            }
        }) {
            let _ = send(&self.writer, &ClientMsg::AttachmentCancel { token: token.clone() });
            self.message(ServerMsg::AttachmentResult {
                pasted: false,
                token,
                message: format!("Cannot start attachment: {error}. Use {action} to retry."),
            });
        }
    }

    fn upload_failed(self: &Arc<Self>, token: String, cancelled: &AtomicBool, error: &anyhow::Error, image: bool) {
        let message = if image {
            format!("Image not attached: {error:#}. Press Ctrl+C to dismiss.")
        } else {
            format!("Attachment stopped: {error:#}. Use Attach files to retry.")
        };
        let dropped = {
            let mut state = self.state.lock().unwrap();
            if let Some(active) = state.active.as_mut().filter(|a| a.token == token) {
                active.failure = Some(message.clone());
            }
            if let Some(dropped) = state.dropped.as_mut().filter(|d| d.token == token) {
                dropped.error = Some(format!("{error:#}"));
                true
            } else {
                false
            }
        };
        {
            let mut writer = self.writer.lock().unwrap();
            if !cancelled.load(Ordering::SeqCst)
                && let Some(writer) = writer.as_mut()
            {
                let _ = protocol::write_json(writer, &ClientMsg::AttachmentCancel { token: token.clone() });
            }
        }
        if !dropped {
            self.message(ServerMsg::AttachmentResult { pasted: false, token, message });
        }
    }

    fn drop_action(self: &Arc<Self>, action: usize) {
        let mut state = self.state.lock().unwrap();
        let Some(mut dropped) = state.dropped.take() else { return };
        if dropped.result {
            if let Some(token) = state.result_token.take() {
                let _ = send(&self.writer, &ClientMsg::AttachmentDismiss { token });
            }
            state.status = None;
            if action == 1 {
                drop(state);
                self.forward(&paste::bracket(&dropped.original));
                self.redraw();
                return;
            }
            if action == 2 {
                drop(state);
                self.redraw();
                return;
            }
            // A retry reserves the current pane again and shows it before anything is sent.
            dropped.token = format!(
                "{:x}-{:x}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()
            );
            dropped.result = false;
            dropped.ready = false;
            dropped.selected = 0;
            dropped.retry = true;
            let token = dropped.token.clone();
            let text = dropped.original.clone();
            state.dropped = Some(dropped);
            drop(state);
            let _ = send(&self.writer, &ClientMsg::AttachmentRequest { token, text });
            self.redraw();
            return;
        }
        if action == 3 {
            state.uncertain = Some(dropped.token.clone());
            drop(state);
            let _ = send(&self.writer, &ClientMsg::AttachmentCancel { token: dropped.token });
            self.redraw();
            return;
        }
        if !dropped.ready {
            state.dropped = Some(dropped);
            return;
        }
        if action == 2 {
            let token = dropped.token.clone();
            let text = dropped.original.clone();
            state.uncertain = Some(token.clone());
            dropped.ready = false;
            state.dropped = Some(dropped);
            drop(state);
            let _ = send(&self.writer, &ClientMsg::AttachmentText { token, text });
            self.redraw();
            return;
        }
        if let Err(error) = dropped.validate() {
            dropped.error = Some(format!("{error:#}"));
            state.dropped = Some(dropped);
            drop(state);
            self.draw();
            return;
        }
        if action == 1
            && let Some((ssh, label)) = &self.destination
            && let Err(error) = paste::remember(&paste::permission_path(), ssh, label)
        {
            dropped.error = Some(format!("Cannot remember this server: {error}. Choose Send to send once."));
            state.dropped = Some(dropped);
            drop(state);
            self.draw();
            return;
        }
        let prompt = Prompt {
            image: false,
            token: dropped.token.clone(),
            target: dropped.target.clone(),
            limits: dropped.limits.clone(),
            text: dropped.paths.iter().map(|p| crate::shell::quote(p.to_str().unwrap())).collect::<Vec<_>>().join(" "),
            cursor: 0,
            error: None,
            paste_failed: false,
            parser: InputParser::new(),
        };
        state.dropped = Some(dropped);
        drop(state);
        self.start(prompt);
        self.redraw();
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
                state.status = Some(if state.dropped.is_some() {
                    active.progress.map_or_else(
                        || "Preparing files. Ctrl+C cancels.".into(),
                        |(received, total)| {
                            format!(
                                "Sending: {:.1} / {:.1} MiB ({}%) [Cancel: Ctrl+C]",
                                received as f64 / 1048576.0,
                                total as f64 / 1048576.0,
                                (received * 100).checked_div(total).unwrap_or(100)
                            )
                        },
                    )
                } else {
                    format!("{progress}. Typing waits. Ctrl+C cancels.")
                });
                drop(state);
                this.draw();
            }
        });
    }

    fn upload(&self, prompt: &Prompt, replies: &Receiver<u64>, cancelled: &AtomicBool) -> Result<()> {
        let started = Instant::now();
        let image = prompt.image.then(|| super::clipboard_image::read(cancelled)).transpose()?;
        let paths = if let Some(image) = &image { vec![image.path.clone()] } else { explicit_paths(&prompt.text)? };
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
        if !self.remote && !prompt.image {
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

    pub fn shutdown(&self) {
        self.disconnect();
        let workers = self.state.lock().unwrap().workers.clone();
        for finished in workers {
            // Signal exit skips destructors, so let the worker reap its helper and remove staging first.
            let _ = finished.recv_timeout(std::time::Duration::from_secs(3));
        }
    }

    pub fn disconnect(&self) {
        let mut state = self.state.lock().unwrap();
        state.negotiated = false;
        state.splitter = Default::default();
        state.parser = InputParser::new();
        if let Some(dropped) = &mut state.dropped {
            dropped.ready = false;
            state.uncertain = Some(dropped.token.clone());
        }
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

impl Dropped {
    fn validate(&self) -> Result<u64> {
        let mut specs = Vec::new();
        for path in &self.paths {
            let meta = std::fs::symlink_metadata(path)?;
            ensure!(
                meta.is_file(),
                "Only regular files can be sent. Use kraal cp for directories, links and special files."
            );
            specs.push(attachments::FileSpec {
                name: path.file_name().and_then(|s| s.to_str()).unwrap_or("").into(),
                size: meta.len(),
            });
        }
        self.limits.check(&specs)
    }

    fn lines(&self, width: usize, server: &str, active: bool, status: Option<&str>) -> Vec<String> {
        let server: String = server.chars().filter(|c| !c.is_control()).collect();
        if active {
            return status
                .map(|text| {
                    vec![if width >= 100 {
                        text.replacen("Sending:", &format!("Sending to {}:", crate::text::truncate(&server, 24)), 1)
                    } else {
                        text.to_string()
                    }]
                })
                .unwrap_or_default();
        }
        if self.result {
            let mut lines =
                crate::text::wrap(status.unwrap_or("Transfer interrupted. Check the pane before retrying."), width);
            lines.push(buttons(&["Retry", "Paste the text", "Close"], self.selected));
            return lines;
        }
        let names = self
            .paths
            .iter()
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        let size = self.validate().unwrap_or(0);
        let noun = if self.paths.len() == 1 { "file" } else { "files" };
        let summary = format!("{} local {noun}, {:.1} MiB: {names}", self.paths.len(), size as f64 / 1048576.0);
        let target = format!("{server} / {}", self.target);
        let mut lines = if width >= 100 { vec![format!("{target} | {summary}")] } else { vec![target, summary] };
        let question = self.error.clone().unwrap_or_else(|| {
            if !self.ready {
                "Preparing attachment. Ctrl+C cancels.".into()
            } else if self.broadcast {
                "Broadcasting is on. Send files and paste paths only to this pane?".into()
            } else {
                "Send the files and paste their remote paths?".into()
            }
        });
        lines.extend(crate::text::wrap(&question, width).into_iter().take(6));
        let always = if width >= 100 {
            format!("Always for {}", crate::text::truncate(&server, 24))
        } else {
            "Always here".into()
        };
        lines.push(buttons(
            &["Send", &always, if width >= 100 { "Paste the text" } else { "Text" }, "Cancel"],
            self.selected,
        ));
        lines
    }
}

fn button_at(line: &str, col: usize) -> Option<usize> {
    let mut start = 1;
    for (index, button) in line.split("] [").enumerate() {
        let width = unicode_width::UnicodeWidthStr::width(button.trim_start_matches('[').trim_end_matches(']')) + 2;
        if (start..start + width).contains(&col) {
            return Some(index);
        }
        start += width + 1;
    }
    None
}

fn buttons(labels: &[&str], selected: usize) -> String {
    labels
        .iter()
        .enumerate()
        .map(|(i, label)| if i == selected { format!("[>{label}<]") } else { format!("[{label}]") })
        .collect::<Vec<_>>()
        .join(" ")
}

fn send(writer: &Writer, msg: &ClientMsg) -> Result<()> {
    let mut writer = writer.lock().unwrap();
    protocol::write_json(writer.as_mut().context("connection closed")?, msg)?;
    Ok(())
}

fn explicit_paths(text: &str) -> Result<Vec<std::path::PathBuf>> {
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
    fn automatic_controller() -> (Arc<Controller>, Reader) {
        let target = super::super::Target::Remote {
            ssh: vec!["alice@preparing-test".into()],
            session: "main".into(),
            label: "box".into(),
            pane: None,
            clipboard_native: false,
        };
        let (writer, peer) = wire();
        let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target);
        let (ssh, label) = client.destination.as_ref().unwrap();
        paste::remember(&paste::permission_path(), ssh, label).unwrap();
        assert!(paste::allowed(&paste::permission_path(), ssh));
        (client, peer)
    }

    fn read(peer: &mut Reader) -> ClientMsg {
        let (kind, data) = protocol::read_frame(peer).unwrap().unwrap();
        assert_eq!(kind, protocol::JSON);
        serde_json::from_slice(&data).unwrap()
    }

    fn dropped_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("kraal-pasted-{name}-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        path
    }

    fn prepare_drop(client: &Arc<Controller>, peer: &mut Reader, original: &[u8]) -> String {
        client.negotiate(true);
        client.input(&paste::bracket(original));
        assert!(matches!(read(peer), ClientMsg::PasteStart));
        let ClientMsg::AttachmentRequest { token, text } = read(peer) else { panic!() };
        assert_eq!(text, original);
        client.message(ServerMsg::AttachmentPrepared {
            token: token.clone(),
            target: "main / Claude".into(),
            limits: Default::default(),
            broadcast: false,
        });
        token
    }

    #[test]
    fn image_offer_requires_local_enter_and_ordinary_paste_never_reads_the_clipboard() {
        for remote in [false, true] {
            let (client, mut peer) = controller(remote);
            client.negotiate(true);
            client.message(ServerMsg::ClipboardImageOffer {
                token: "ab".into(),
                target: "pane".into(),
                limits: Default::default(),
            });
            assert!(peer.incoming.try_recv().is_err());
            client.keys(b"\x16\x1b[200~\r\x1b[201~");
            assert!(client.state.lock().unwrap().active.is_none());
            assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().image);
            assert!(peer.incoming.try_recv().is_err());
            client.keys(b"\x03");
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { .. }));
            assert!(client.state.lock().unwrap().prompt.is_none());
        }
    }

    #[test]
    fn image_offer_between_paste_fragments_requires_enter_outside_the_paste() {
        if !crate::testing::in_own_process(
            "client::attachments::tests::image_offer_between_paste_fragments_requires_enter_outside_the_paste",
        ) {
            return;
        }
        let root = crate::paths::state_dir().join("fragmented-image-test");
        std::fs::create_dir_all(&root).unwrap();
        for tool in ["wl-paste", "xclip", "osascript"] {
            crate::testing::write_script(&root.join(tool), "#!/bin/sh\nexit 1\n");
        }
        // This process has not started any workers yet. Never read the desktop clipboard.
        unsafe {
            std::env::set_var("PATH", &root);
            std::env::set_var("WAYLAND_DISPLAY", "test");
        }
        for text in [b"text".to_vec(), vec![b'x'; paste::LIMIT + 1]] {
            let bytes = paste::bracket(&[text, b"\r".to_vec()].concat());
            for split in [1, 5, 6, bytes.len() - 7, bytes.len() - 3] {
                let (client, _peer) = controller(true);
                client.negotiate(true);
                client.input(&bytes[..split]);
                client.message(ServerMsg::ClipboardImageOffer {
                    token: "ab".into(),
                    target: "pane".into(),
                    limits: Default::default(),
                });
                client.input(&bytes[split..]);
                let state = client.state.lock().unwrap();
                assert!(state.active.is_none(), "paste authorized an image at split {split}");
                assert!(state.prompt.as_ref().is_some_and(|p| p.image), "prompt lost at split {split}");
                drop(state);
                client.input(b"\r");
                assert!(client.state.lock().unwrap().prompt.is_none());
                client.shutdown();
            }
        }
    }

    #[test]
    fn images_read_once_and_use_acknowledged_chunks_even_for_local_sessions() {
        if !crate::testing::in_own_process(
            "client::attachments::tests::images_read_once_and_use_acknowledged_chunks_even_for_local_sessions",
        ) {
            return;
        }
        let root = crate::paths::state_dir().join("image-test");
        std::fs::create_dir_all(&root).unwrap();
        let log = root.join("reads");
        let temporary = root.join("tmp");
        std::fs::create_dir_all(&temporary).unwrap();
        #[cfg(not(target_os = "macos"))]
        let (tool, script) =
            ("wl-paste", format!("#!/bin/sh\necho read >> '{}'\nprintf '\\211PNG\\r\\n\\032\\n'\n", log.display()));
        #[cfg(target_os = "macos")]
        let (tool, script) = (
            "osascript",
            format!(
                "#!/bin/sh\necho read >> '{}'\nfor arg; do file=$arg; done\nprintf '\\211PNG\\r\\n\\032\\n' > \"$file\"\n",
                log.display()
            ),
        );
        crate::testing::write_script(&root.join(tool), &script);
        // This test runs alone, before starting clipboard or attachment workers.
        unsafe {
            std::env::set_var("PATH", format!("{}:{}", root.display(), std::env::var("PATH").unwrap()));
            std::env::set_var("TMPDIR", &temporary);
            std::env::set_var("WAYLAND_DISPLAY", "test");
            std::env::remove_var("DISPLAY");
        }
        for remote in [false, true] {
            let (client, mut peer) = controller(remote);
            client.message(ServerMsg::ClipboardImageOffer {
                token: "ab".into(),
                target: "pane".into(),
                limits: Default::default(),
            });
            client.keys(b"\r");
            let files = loop {
                match read(&mut peer) {
                    ClientMsg::AttachmentStart { files, .. } => break files,
                    ClientMsg::Resize { .. } => {}
                    _ => panic!("image did not use attachment transport"),
                }
            };
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].name, "clipboard.png");
            assert_eq!(files[0].size, 8);
            client.message(ServerMsg::AttachmentAck { token: "ab".into(), received: 0 });
            let ClientMsg::AttachmentChunk { data, .. } = read(&mut peer) else { panic!() };
            assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), b"\x89PNG\r\n\x1a\n");
            client.message(ServerMsg::AttachmentAck { token: "ab".into(), received: 8 });
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentFinish { .. }));
            client.shutdown();
        }
        assert_eq!(std::fs::read_to_string(log).unwrap(), "read\nread\n");
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
        let ready = root.join("ready");
        crate::testing::write_script(
            &root.join(tool),
            &format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n", ready.display()),
        );
        for stop in [None, Some(Controller::cancel as fn(&Controller)), Some(Controller::disconnect)] {
            let (client, _peer) = controller(true);
            client.message(ServerMsg::ClipboardImageOffer {
                token: "ab".into(),
                target: "pane".into(),
                limits: Default::default(),
            });
            client.keys(b"\r");
            let started = Instant::now();
            let pid: libc::pid_t = loop {
                if let Some(pid) = std::fs::read_to_string(&ready).ok().and_then(|text| text.trim().parse().ok()) {
                    break pid;
                }
                assert!(started.elapsed() < crate::testing::PATIENCE);
                std::thread::sleep(Duration::from_millis(5));
            };
            if let Some(stop) = stop {
                stop(&client);
            }
            client.shutdown();
            assert!(client.state.lock().unwrap().active.is_none());
            assert_eq!(
                std::fs::read_dir(&temporary).unwrap().count(),
                0,
                "shutdown left an unfinished clipboard export"
            );
            // SAFETY: signal zero only checks whether the helper still exists.
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "shutdown left the clipboard helper running");
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
            std::fs::remove_file(&ready).unwrap();
        }
    }

    #[test]
    fn concurrent_shutdowns_wait_for_cancelled_workers_to_finish_cleanup() {
        for stop in [Controller::cancel as fn(&Controller), Controller::disconnect] {
            let (client, _peer) = controller(true);
            let (finished, completion) = bounded(1);
            let cancelled = Arc::new(AtomicBool::new(false));
            {
                let mut state = client.state.lock().unwrap();
                state.workers.push(completion);
                state.active = Some(Active {
                    token: "ab".into(),
                    cancelled: cancelled.clone(),
                    ack: bounded(1).0,
                    started: Instant::now(),
                    progress: None,
                    failure: None,
                });
            }
            stop(&client);
            assert!(cancelled.load(Ordering::SeqCst));
            assert!(client.state.lock().unwrap().active.is_none());
            let (entered, entering) = bounded(2);
            let (done, completed) = bounded(2);
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    let client = client.clone();
                    let entered = entered.clone();
                    let done = done.clone();
                    std::thread::spawn(move || {
                        entered.send(()).unwrap();
                        client.shutdown();
                        done.send(()).unwrap();
                    })
                })
                .collect();
            for _ in 0..2 {
                entering.recv_timeout(crate::testing::PATIENCE).unwrap();
            }
            // The worker still owns its completion sender while its destructors run.
            let early = completed.recv_timeout(Duration::from_millis(100));
            drop(finished);
            for thread in threads {
                thread.join().unwrap();
            }
            assert!(matches!(early, Err(crossbeam_channel::RecvTimeoutError::Timeout)));
            assert_eq!(completed.len(), 2);
        }
    }

    #[test]
    fn always_is_local_reused_on_next_attach_and_revoked_before_the_next_paste() {
        if !crate::testing::in_own_process(
            "client::attachments::tests::always_is_local_reused_on_next_attach_and_revoked_before_the_next_paste",
        ) {
            return;
        }
        let path = dropped_file("permission");
        let ssh = vec!["alice@box".into()];
        let target = super::super::Target::Remote {
            ssh: ssh.clone(),
            session: "main".into(),
            label: "box".into(),
            pane: None,
            clipboard_native: false,
        };
        let (writer, mut peer) = wire();
        let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target);
        prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        assert!(!paste::allowed(&paste::permission_path(), &ssh));
        client.keys(b"\t\r");
        assert!(paste::allowed(&paste::permission_path(), &ssh));
        assert!(client.state.lock().unwrap().active.is_some());
        client.disconnect();
        for (broadcast, revoke) in [(false, false), (true, false), (false, true)] {
            if revoke {
                std::fs::remove_file(paste::permission_path()).unwrap();
            }
            let (writer, mut peer) = wire();
            let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target);
            client.negotiate(true);
            // A server cannot create a local gesture, even with permission already granted.
            client.message(ServerMsg::AttachmentPrepared {
                token: "ab".into(),
                target: "pane".into(),
                limits: Default::default(),
                broadcast: false,
            });
            assert!(client.state.lock().unwrap().active.is_none());
            client.input(&paste::bracket(path.to_str().unwrap().as_bytes()));
            assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
            let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
            client.message(ServerMsg::AttachmentPrepared {
                token,
                target: "main / Claude".into(),
                limits: Default::default(),
                broadcast,
            });
            assert_eq!(client.state.lock().unwrap().active.is_some(), !broadcast && !revoke);
            client.disconnect();
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn paste_recognition_requires_remote_negotiation_and_preserves_ordinary_bytes() {
        let path = dropped_file("ordinary");
        for remote in [false, true] {
            let (client, peer) = controller(remote);
            let input = paste::bracket(path.to_str().unwrap().as_bytes());
            assert!(!client.input(&input));
            assert!(peer.incoming.try_recv().is_err());
            if !remote {
                client.negotiate(true);
                assert!(!client.input(&input));
            }
        }
        let (client, mut peer) = controller(true);
        client.negotiate(true);
        for original in [b"echo /some/path".as_slice(), b"/does/not/exist", b"\xff", b"", b"multiple\nlines"] {
            let input = paste::bracket(original);
            for byte in &input {
                assert!(client.input(&[*byte]));
            }
            if !original.is_empty() {
                assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
            }
            let (kind, data) = protocol::read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(kind, protocol::BYTES);
            assert_eq!(data, input);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn original_text_is_kept_and_a_coalesced_tail_cannot_authorize_sending() {
        let path = dropped_file("text with spaces");
        let original = format!("  '{}'\r\n", path.display()).into_bytes();
        let (client, mut peer) = controller(true);
        client.negotiate(true);
        client.input(&[paste::bracket(&original), b"\r".to_vec()].concat());
        assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
        let ClientMsg::AttachmentRequest { token, text } = read(&mut peer) else { panic!() };
        assert_eq!(text, original);
        let (kind, tail) = protocol::read_frame(&mut peer).unwrap().unwrap();
        assert_eq!(kind, protocol::BYTES);
        assert_eq!(tail, b"\r");
        client.message(ServerMsg::AttachmentPrepared {
            token: token.clone(),
            target: "main / Claude".into(),
            limits: Default::default(),
            broadcast: false,
        });
        assert!(client.state.lock().unwrap().active.is_none());
        assert!(peer.incoming.try_recv().is_err());
        client.keys(b"\t\t\r");
        assert!(
            matches!(read(&mut peer), ClientMsg::AttachmentText { token: sent, text } if sent == token && text == original)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn first_choice_action_keeps_coalesced_trailing_input() {
        let path = dropped_file("choice-tail");
        for (keys, cancel) in [(b"\x03\r".as_slice(), true), (b"\rafter".as_slice(), false)] {
            let (client, mut peer) = controller(true);
            let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
            client.input(keys);
            let mut started = false;
            let mut cancelled = false;
            let mut tail = Vec::new();
            // The action, redraw, and trailing bytes each produce a frame.
            for _ in 0..3 {
                let (kind, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
                if kind == protocol::BYTES {
                    tail.extend(bytes);
                } else {
                    match serde_json::from_slice::<ClientMsg>(&bytes).unwrap() {
                        ClientMsg::AttachmentStart { token: sent, .. } => {
                            assert_eq!(sent, token);
                            started = true;
                        }
                        ClientMsg::AttachmentCancel { token: sent } => {
                            assert_eq!(sent, token);
                            cancelled = true;
                        }
                        ClientMsg::Resize { .. } => {}
                        other => panic!("unexpected message: {other:?}"),
                    }
                }
            }
            assert_eq!(cancelled, cancel);
            assert_eq!(started, !cancel);
            assert_eq!(tail, if cancel { b"\r".as_slice() } else { b"after" });
            assert_eq!(client.state.lock().unwrap().active.is_some(), !cancel);
            client.disconnect();
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancellation_before_prepared_prevents_automatic_sending() {
        if !crate::testing::in_own_process(
            "client::attachments::tests::cancellation_before_prepared_prevents_automatic_sending",
        ) {
            return;
        }
        let path = dropped_file("cancel-preparing");
        for automatic in [false, true] {
            for coalesced in [false, true] {
                for cancel in [b"\x03".as_slice(), b"\x1b[99;5u"] {
                    let (client, mut peer) = if automatic { automatic_controller() } else { controller(true) };
                    client.negotiate(true);
                    let paste = paste::bracket(path.to_str().unwrap().as_bytes());
                    if coalesced {
                        client.input(&[paste.as_slice(), cancel, b"\r"].concat());
                    } else {
                        client.input(&paste);
                        for byte in cancel {
                            client.input(std::slice::from_ref(byte));
                        }
                        client.input(b"\r");
                    }
                    assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
                    let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
                    loop {
                        let (kind, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
                        if kind == protocol::JSON {
                            assert!(matches!(serde_json::from_slice::<ClientMsg>(&bytes).unwrap(),
                            ClientMsg::AttachmentCancel { token: sent } if sent == token));
                            break;
                        }
                        assert!(!bytes.contains(&3));
                    }
                    assert!(matches!(read(&mut peer), ClientMsg::Resize { .. }));
                    assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"\r".to_vec()));
                    client.message(ServerMsg::AttachmentPrepared {
                        token,
                        target: "main / Claude".into(),
                        limits: Default::default(),
                        broadcast: false,
                    });
                    let state = client.state.lock().unwrap();
                    assert!(state.dropped.is_none());
                    assert!(state.active.is_none());
                    assert!(peer.incoming.try_recv().is_err());
                }
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn consecutive_pastes_are_forwarded_while_preparing() {
        if !crate::testing::in_own_process(
            "client::attachments::tests::consecutive_pastes_are_forwarded_while_preparing",
        ) {
            return;
        }
        let path = dropped_file("consecutive");
        for automatic in [false, true] {
            for coalesced in [false, true] {
                let (client, mut peer) = if automatic { automatic_controller() } else { controller(true) };
                client.negotiate(true);
                let first = paste::bracket(path.to_str().unwrap().as_bytes());
                let second = paste::bracket(b"Please review this file.\n");
                if coalesced {
                    client.input(&[first, second.clone()].concat());
                } else {
                    client.input(&first);
                    client.input(&second);
                }
                assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
                let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, second));
                assert!(peer.incoming.try_recv().is_err());
                client.message(ServerMsg::AttachmentPrepared {
                    token,
                    target: "main / Claude".into(),
                    limits: Default::default(),
                    broadcast: false,
                });
                assert_eq!(client.state.lock().unwrap().active.is_some(), automatic);
                client.disconnect();
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn text_choice_preserves_separate_reads_before_its_result() {
        let path = dropped_file("text-tail");
        let (client, mut peer) = controller(true);
        let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        client.input(b"\t\t\r");
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentText { token: sent, .. } if sent == token));
        assert!(matches!(read(&mut peer), ClientMsg::Resize { .. }));
        for bytes in [b"later".as_slice(), b" typing", b"\r", &paste::bracket(b"instructions")] {
            client.input(bytes);
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, bytes.to_vec()));
        }
        assert!(client.state.lock().unwrap().uncertain.is_some());
        client.message(ServerMsg::AttachmentResult { token, message: "Text submitted.".into(), pasted: true });
        assert!(client.state.lock().unwrap().dropped.is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn immediate_cancellation_reply_keeps_the_local_upload_error() {
        struct ReplyOnCancel {
            client: std::sync::Weak<Controller>,
            bytes: Vec<u8>,
            reply: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
        }
        impl Write for ReplyOnCancel {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                let mut input = std::io::Cursor::new(&self.bytes);
                if let Ok(Some((protocol::JSON, json))) = protocol::read_frame(&mut input)
                    && let Ok(ClientMsg::AttachmentCancel { token }) = serde_json::from_slice(&json)
                {
                    let client = self.client.upgrade().unwrap();
                    // Deliver the reply before the cancellation write returns.
                    {
                        let state = client.state.lock().unwrap();
                        if let Some(dropped) = &state.dropped {
                            dropped.error.as_ref().expect("error before cancel");
                        } else {
                            state.active.as_ref().unwrap().failure.as_ref().expect("image error before cancel");
                        }
                    }
                    let reply = client.clone();
                    let handle = std::thread::spawn(move || {
                        reply.message(ServerMsg::AttachmentResult {
                            token,
                            message: "Transfer interrupted: cancelled.".into(),
                            pasted: false,
                        })
                    });
                    let started = Instant::now();
                    while client.state.lock().unwrap().status.is_none() {
                        assert!(started.elapsed() < Duration::from_secs(2));
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    // Redraw finishes after this write releases the writer.
                    *self.reply.lock().unwrap() = Some(handle);
                    self.bytes.clear();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for image in [false, true] {
            let path = dropped_file("immediate-error");
            let (client, mut peer) = controller(true);
            let token = if image {
                client.state.lock().unwrap().active = Some(Active {
                    token: "ab".into(),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    ack: bounded(2).0,
                    started: Instant::now(),
                    progress: None,
                    failure: None,
                });
                "ab".to_string()
            } else {
                prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes())
            };
            let reply = Arc::new(Mutex::new(None));
            *client.writer.lock().unwrap() = Some(Box::new(ReplyOnCancel {
                client: Arc::downgrade(&client),
                bytes: Vec::new(),
                reply: reply.clone(),
            }));
            std::fs::remove_file(&path).unwrap();
            let error = Source::open(path).err().unwrap();
            client.upload_failed(token, &AtomicBool::new(false), &error, image);
            reply.lock().unwrap().take().unwrap().join().unwrap();
            let state = client.state.lock().unwrap();
            let status = state.status.as_ref().unwrap();
            assert!(status.contains(&format!("{error:#}")));
            if image {
                assert!(status.contains("Image not attached:"));
                assert!(status.contains("Ctrl+C to dismiss"));
            } else {
                assert!(status.contains("Transfer interrupted: cancelled."));
            }
        }
    }

    #[test]
    fn failed_drop_retains_text_retry_and_close_and_success_does_not_offer_retry() {
        for action in [0, 1, 2] {
            let path = dropped_file(&format!("failure-{action}"));
            let original = format!("'{}'", path.display()).into_bytes();
            let (client, mut peer) = controller(true);
            let token = prepare_drop(&client, &mut peer, &original);
            client.message(ServerMsg::AttachmentResult {
                token: token.clone(),
                message: "Transfer interrupted. No paths pasted.".into(),
                pasted: false,
            });
            assert!(matches!(read(&mut peer), ClientMsg::Resize { .. }));
            client.drop_action(action);
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { token: sent } if sent == token));
            if action == 0 {
                assert!(matches!(read(&mut peer), ClientMsg::AttachmentRequest { text, .. } if text == original));
            } else if action == 1 {
                let (kind, text) = protocol::read_frame(&mut peer).unwrap().unwrap();
                assert_eq!(kind, protocol::BYTES);
                assert_eq!(text, paste::bracket(&original));
            }
            std::fs::remove_file(path).unwrap();
        }
        let path = dropped_file("success");
        let (client, mut peer) = controller(true);
        let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        client.message(ServerMsg::AttachmentResult { token, message: "Files received.".into(), pasted: true });
        assert!(client.state.lock().unwrap().dropped.is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn directories_links_special_files_and_limits_are_refused_before_sending() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("kraal-drop-refusal-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("file");
        std::fs::write(&file, b"abc").unwrap();
        symlink(&file, root.join("link")).unwrap();
        let fifo = std::ffi::CString::new(root.join("fifo").to_str().unwrap()).unwrap();
        // SAFETY: a valid C path, created inside this test directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for path in [&root, &root.join("link"), &root.join("fifo")] {
            let (client, mut peer) = controller(true);
            prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
            assert!(
                client.state.lock().unwrap().dropped.as_ref().unwrap().error.as_ref().unwrap().contains("kraal cp")
            );
            client.keys(b"\r");
            assert!(client.state.lock().unwrap().active.is_none());
            assert!(peer.incoming.try_recv().is_err());
        }
        let (client, mut peer) = controller(true);
        prepare_drop(&client, &mut peer, file.to_str().unwrap().as_bytes());
        client.state.lock().unwrap().dropped.as_mut().unwrap().limits.file_bytes = 2;
        client.keys(b"\r");
        assert!(client.state.lock().unwrap().active.is_none());
        assert!(peer.incoming.try_recv().is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn drop_layouts_at_124_and_80_columns_keep_all_four_choices() {
        let path = dropped_file("layout");
        let (client, mut peer) = controller(true);
        prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        let state = client.state.lock().unwrap();
        let dropped = state.dropped.as_ref().unwrap();
        let wide = dropped.lines(122, "SERVER", false, None);
        assert!(wide[0].starts_with("SERVER / main / Claude | 1 local file, 0.0 MiB:"));
        assert_eq!(wide[1], "Send the files and paste their remote paths?");
        assert_eq!(wide[2], "[>Send<] [Always for SERVER] [Paste the text] [Cancel]");
        let narrow = dropped.lines(78, "SERVER", false, None);
        assert_eq!(narrow[0], "SERVER / main / Claude");
        assert_eq!(narrow[3], "[>Send<] [Always here] [Text] [Cancel]");
        assert!(dropped.lines(78, "SERVER", true, None).is_empty());
        assert_eq!(
            dropped.lines(78, "SERVER", true, Some("Sending: 1 / 3 MiB (33%) [Cancel: Ctrl+C]")),
            ["Sending: 1 / 3 MiB (33%) [Cancel: Ctrl+C]"]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn mouse_choices_and_progress_cancellation_use_the_displayed_buttons() {
        let path = dropped_file("mouse");
        let (client, mut peer) = controller(true);
        let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let lines = client.state.lock().unwrap().dropped.as_ref().unwrap().lines(
            usize::from(cols).saturating_sub(2).max(1),
            "server",
            false,
            None,
        );
        let line = lines.last().unwrap();
        let column = line.find("Text").or_else(|| line.find("Paste the text")).unwrap() + 2;
        assert_eq!(button_at(line, column - 1), Some(2));
        client.keys(format!("\x1b[<0;{column};{rows}M").as_bytes());
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentText { token: sent, .. } if sent == token));
        let (client, mut peer) = controller(true);
        let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        let cancelled = Arc::new(AtomicBool::new(false));
        let status = "Sending: 0.0 / 3.2 MiB (0%) [Cancel: Ctrl+C]";
        {
            let mut state = client.state.lock().unwrap();
            state.active = Some(Active {
                token: token.clone(),
                cancelled: cancelled.clone(),
                ack: bounded(2).0,
                started: Instant::now(),
                progress: None,
                failure: None,
            });
            state.status = Some(status.into());
        }
        let lines = client.state.lock().unwrap().dropped.as_ref().unwrap().lines(
            usize::from(cols).saturating_sub(2).max(1),
            "server",
            true,
            Some(status),
        );
        let column = lines[0].find("Cancel").unwrap() + 2;
        client.keys(format!("\x1b[<0;{column};{rows}M").as_bytes());
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { token: sent } if sent == token));
        assert!(cancelled.load(Ordering::SeqCst));
        std::fs::remove_file(path).unwrap();
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
                                failure: None,
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
            image: false,
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
            image: false,
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
            image: false,
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
            failure: None,
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
            image: false,
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
            image: false,
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
                failure: None,
            });
        }
        client.message(ServerMsg::AttachmentResult {
            pasted: true,
            token: "ab".into(),
            message: "Files received.".into(),
        });
        let state = client.state.lock().unwrap();
        assert_eq!(state.active.as_ref().unwrap().token, "cd");
        assert!(!cancelled.load(Ordering::SeqCst));
        assert!(state.uncertain.is_none());
        assert!(state.status.is_none());
    }
}
