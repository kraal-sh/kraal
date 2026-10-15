//! The server can offer a form, but only local keyboard input authorizes opening files.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::server::overlays;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use crossbeam_channel::{Receiver, Sender, bounded};

use super::clipboard_image::{Diagnostic, Notices};
use super::paste;
use crate::attachments::{self, Source};
use crate::input::{InputEvent, InputParser, KeyCode};
use crate::protocol::{self, ClientMsg, ServerMsg};
use crate::theme::NoticeKind;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const SUCCESS_DURATION: Duration = Duration::from_millis(1500);

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
    last_draw: Option<Instant>,
    label: String,
    progress: Option<(u64, u64)>,
    failure: Option<String>,
}

impl Active {
    fn spinner(&self, theme: crate::theme::Theme, now: Instant) -> char {
        let frames = if theme.ascii {
            &['|', '/', '-', '\\'][..]
        } else {
            &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'][..]
        };
        let frame = (now.saturating_duration_since(self.started).as_millis() / PROGRESS_INTERVAL.as_millis()) as usize;
        frames[frame % frames.len()]
    }

    fn status(&self, theme: crate::theme::Theme, now: Instant) -> String {
        if self.progress.is_none() {
            format!("{} Preparing attachment.\nTyping waits until it finishes.", self.spinner(theme, now))
        } else {
            "Typing waits until it finishes.".into()
        }
    }

    fn transfer(&self, theme: crate::theme::Theme, now: Instant, done: bool) -> Option<TransferProgress> {
        let (received, total) = self.progress?;
        Some(TransferProgress {
            label: self.label.chars().filter(|c| !c.is_control()).collect(),
            received: if done { total } else { received },
            total,
            marker: if done { if theme.ascii { 'v' } else { '✓' } } else { self.spinner(theme, now) },
            ascii: theme.ascii,
        })
    }
}

#[derive(Clone)]
pub(super) struct TransferProgress {
    label: String,
    received: u64,
    total: u64,
    marker: char,
    ascii: bool,
}

impl TransferProgress {
    fn message(&self, width: usize) -> String {
        let percent = (self.received.saturating_mul(100)).checked_div(self.total).unwrap_or(100).min(100);
        let bar_width = width.saturating_sub(2);
        let filled = (percent as usize * bar_width + 50) / 100;
        let (on, off) = if self.ascii { ("#", "-") } else { ("▓", "░") };
        format!(
            "{}\n{} / {} ({percent}%)\n{} {}{}",
            crate::text::truncate(&self.label, 64),
            crate::text::human_size(self.received),
            crate::text::human_size(self.total),
            self.marker,
            on.repeat(filled),
            off.repeat(bar_width - filled),
        )
    }
}

struct Dropped {
    image: bool,
    shortcut: bool,
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
    scroll: usize,
    error: Option<String>,
}

#[derive(Default)]
struct State {
    dropped: Option<Dropped>,
    connected: bool,
    focus: Option<(Option<crate::layout::PaneId>, bool)>,
    image_approved: bool,
    image_probe: Option<Receiver<()>>,
    image_notices: Notices,
    image_keys: super::clipboard_image::Keys,
    splitter: paste::Splitter,
    prompt: Option<Prompt>,
    active: Option<Active>,
    workers: Vec<Receiver<()>>,
    status: Option<String>,
    success_until: Option<Instant>,
    completed: Option<TransferProgress>,
    displayed: Option<TransferProgress>,
    drawn_area: Option<Rect>,
    status_kind: NoticeKind,
    uncertain: Option<String>,
    result_token: Option<String>,
    parser: InputParser,
}

pub(crate) struct Controller {
    state: Mutex<State>,
    writer: Writer,
    remote: bool,
    destination: Option<(Vec<String>, String)>,
    laptop: Option<Arc<super::laptop::Controller>>,
}

impl Controller {
    #[cfg(test)]
    pub fn new(writer: Writer, remote: bool) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::default()), writer, remote, destination: None, laptop: None })
    }

    pub fn for_target(
        writer: Writer,
        target: &super::Target,
        laptop: Option<Arc<super::laptop::Controller>>,
    ) -> Arc<Self> {
        let destination = match target {
            super::Target::Remote { ssh, label, .. } => Some((ssh.clone(), label.clone())),
            super::Target::Local { .. } => None,
        };
        Arc::new(Self {
            state: Mutex::new(State::default()),
            writer,
            remote: destination.is_some(),
            destination,
            laptop,
        })
    }

    pub fn connected(&self) {
        self.state.lock().unwrap().connected = true;
    }

    pub fn pending_keys(&self) -> bool {
        let state = self.state.lock().unwrap();
        self.laptop.as_ref().is_some_and(|laptop| laptop.pending_keys())
            || state.image_keys.pending()
            || state.splitter.has_pending()
            || state.dropped.is_some() && state.parser.has_pending()
            || state.prompt.as_ref().is_some_and(|p| p.parser.has_pending())
    }

    pub fn input(self: &Arc<Self>, bytes: &[u8]) -> bool {
        self.input_with_probe(bytes, super::clipboard_image::available)
    }

    fn input_with_probe(
        self: &Arc<Self>,
        bytes: &[u8],
        available: impl Fn() -> Result<bool, Diagnostic> + Clone + Send + 'static,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.connected && state.dropped.is_some() {
            return false;
        }
        if !self.remote || !state.connected {
            drop(state);
            return self.keys(bytes);
        }
        let parts = if bytes.is_empty() {
            vec![paste::Part::Bytes(state.splitter.idle()), paste::Part::Bytes(Vec::new())]
        } else {
            state.splitter.feed(bytes)
        };
        drop(state);
        // Both parsers see the uninterrupted stream before either dialog consumes input.
        if self.laptop.as_ref().is_some_and(|laptop| laptop.input(bytes)) {
            return true;
        }
        let mut following_paste = false;
        for part in parts {
            if matches!(&part, paste::Part::Start | paste::Part::Paste(_)) {
                let pending = self.state.lock().unwrap().image_keys.feed(&[]);
                for (raw, _) in pending {
                    if following_paste {
                        self.preparing_keys(&raw);
                    } else if !self.keys(&raw) {
                        self.forward(&raw);
                    }
                }
            }
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
                    let parts = self.state.lock().unwrap().image_keys.feed(&bytes);
                    for (raw, image_key) in parts {
                        if following_paste {
                            self.preparing_keys(&raw);
                        } else if image_key && self.offer_clipboard(&raw, &available) {
                            following_paste = true;
                        } else if !self.keys(&raw) {
                            self.forward(&raw);
                        }
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
                            image: false,
                            shortcut: false,
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
                            scroll: 0,
                            error: None,
                        });
                        state.status = None;
                        state.success_until = None;
                        let dismiss = state.result_token.take();
                        state.parser = InputParser::new();
                        drop(state);
                        if let Some(token) = dismiss {
                            let _ = send(&self.writer, &ClientMsg::AttachmentDismiss { token });
                        }
                        let _ =
                            send(&self.writer, &ClientMsg::AttachmentRequest { token, text, image: false, pane: None });
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

    fn offer_clipboard(
        self: &Arc<Self>,
        raw: &[u8],
        available: &(impl Fn() -> Result<bool, Diagnostic> + Clone + Send + 'static),
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        let eligible = self.remote
            && state.connected
            && state.focus.is_some_and(|(pane, _)| pane.is_some())
            && state.prompt.is_none()
            && state.dropped.is_none()
            && state.active.is_none()
            && state.uncertain.is_none();
        let focus = state.focus;
        if !eligible {
            return false;
        }
        if !focus.is_some_and(|(_, agent)| agent) {
            if state
                .image_probe
                .as_ref()
                .is_some_and(|done| matches!(done.try_recv(), Err(crossbeam_channel::TryRecvError::Empty)))
            {
                return false;
            }
            let (finished, done) = bounded(1);
            state.image_probe = Some(done);
            drop(state);
            let available = available.clone();
            let this = Arc::downgrade(self);
            let _ = crate::threads::named("clipboard-probe").spawn(move || {
                let availability = available();
                if let Some(this) = this.upgrade() {
                    let mut state = this.state.lock().unwrap();
                    if matches!(availability, Ok(true))
                        && state.connected
                        && state.focus == focus
                        && state.view(&this.host()).is_none()
                    {
                        this.show_image_notice(&mut state, Diagnostic::NotAgent);
                    }
                }
                let _ = finished.send(());
            });
            return false;
        }
        drop(state);
        let availability = available();
        let mut state = self.state.lock().unwrap();
        // A reply arriving during the type probe cannot authorize an image or replace a question.
        if state.prompt.is_some()
            || state.dropped.is_some()
            || state.active.is_some()
            || state.uncertain.is_some()
            || state.focus != focus
            || !state.connected
        {
            return false;
        }
        match availability {
            Ok(true) => {}
            result => {
                drop(state);
                self.image_notice(result.err().unwrap_or(Diagnostic::NoImage));
                return false;
            }
        }
        let token = format!(
            "{:x}-{:x}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()
        );
        state.dropped = Some(Dropped {
            image: true,
            shortcut: true,
            token: token.clone(),
            original: raw.to_vec(),
            paths: Vec::new(),
            target: String::new(),
            limits: Default::default(),
            broadcast: false,
            ready: false,
            result: false,
            retry: false,
            selected: 0,
            scroll: 0,
            error: None,
        });
        state.status = None;
        state.success_until = None;
        state.parser = InputParser::new();
        if let Some(token) = state.result_token.take() {
            let _ = send(&self.writer, &ClientMsg::AttachmentDismiss { token });
        }
        let _ = send(
            &self.writer,
            &ClientMsg::AttachmentRequest {
                token,
                text: raw.to_vec(),
                image: true,
                pane: focus.and_then(|(pane, _)| pane),
            },
        );
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
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let screen = Rect::new(0, 0, cols, rows.saturating_sub(1));
        for (index, byte) in bytes.iter().enumerate() {
            let events = state.parser.feed(std::slice::from_ref(byte));
            let cancel = events.iter().any(|event| match event {
                InputEvent::Key(key) => key.code == KeyCode::Char('c') && key.mods.ctrl,
                InputEvent::Mouse(mouse) => {
                    mouse.kind == crate::input::MouseKind::Press(crate::input::MouseButton::Left)
                        && self.button_at(&state, screen, mouse.col, mouse.row).is_some()
                }
                _ => false,
            });
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

    fn host(&self) -> String {
        self.destination
            .as_ref()
            .map_or("this machine", |(_, label)| label)
            .chars()
            .filter(|c| !c.is_control())
            .collect()
    }

    fn output_local(&self, bytes: &[u8]) -> std::io::Result<bool> {
        match &self.laptop {
            Some(laptop) => laptop.output(bytes),
            None => super::terminal().write(bytes),
        }
    }

    pub fn draw(&self) {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        self.draw_with(cols, rows, |bytes| {
            let _ = self.output_local(bytes);
        });
    }

    fn draw_with(&self, cols: u16, rows: u16, write: impl FnOnce(&[u8])) {
        // Keep the snapshot and its terminal write ordered with state changes and other draws.
        let mut state = self.state.lock().unwrap();
        if let Some(view) = state.view(&self.host()) {
            let (buf, _) = view.render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
            write(&self.overlay(&mut state, &buf));
        } else {
            state.drawn_area = None;
            write(&state.image_notices.render(cols, rows));
        }
    }

    fn overlay(&self, state: &mut State, buf: &Buffer) -> Vec<u8> {
        if state.drawn_area.replace(buf.area).is_some_and(|previous| previous.intersection(buf.area) != previous) {
            self.redraw();
        }
        crate::ansi::overlay(buf)
    }

    pub fn output(&self, bytes: &[u8]) -> std::io::Result<bool> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        self.output_with(bytes, cols, rows, |bytes| self.output_local(bytes))
    }

    fn output_with(
        &self,
        bytes: &[u8],
        cols: u16,
        rows: u16,
        write: impl FnOnce(&[u8]) -> std::io::Result<bool>,
    ) -> std::io::Result<bool> {
        // A server frame and the local question are one write, so incoming output cannot
        // briefly replace the question while a local key is authorizing a transfer.
        let mut state = self.state.lock().unwrap();
        let view = state.view(&self.host());
        let notice = state.image_notices.render(cols, rows);
        if view.is_none() {
            state.drawn_area = None;
        }
        if view.is_none() && notice.is_empty() {
            return write(bytes);
        }
        let mut out = b"\x1b[?2026h".to_vec();
        // The server ends its own synchronized frame. Keep the local box in that frame.
        for part in bytes.split_inclusive(|b| *b == b'l') {
            out.extend_from_slice(part.strip_suffix(b"\x1b[?2026l").unwrap_or(part));
        }
        if let Some(view) = view {
            let (buf, _) = view.render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
            out.extend(self.overlay(&mut state, &buf));
        } else {
            out.extend(notice);
        }
        out.extend_from_slice(b"\x1b[?2026l");
        write(&out)
    }

    fn button_at(&self, state: &State, screen: Rect, col: u16, row: u16) -> Option<usize> {
        let (_, buttons) = state.view(&self.host())?.render(screen);
        buttons.iter().position(|area| area.contains((col, row).into()))
    }

    fn image_notice(self: &Arc<Self>, diagnostic: Diagnostic) {
        let mut state = self.state.lock().unwrap();
        self.show_image_notice(&mut state, diagnostic);
    }

    fn show_image_notice(self: &Arc<Self>, state: &mut State, diagnostic: Diagnostic) {
        let replacing = state.image_notices.is_active();
        if !state.image_notices.show(diagnostic) {
            return;
        }
        // A full server frame restores every row covered by the previous notice.
        if replacing {
            self.redraw();
        }
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        if state.view(&self.host()).is_none() {
            let _ = self.output_local(&state.image_notices.render(cols, rows));
        }
        let this = Arc::downgrade(self);
        let _ = crate::threads::named("clipboard-notice").spawn(move || {
            std::thread::sleep(Duration::from_secs(4));
            if let Some(this) = this.upgrade() {
                this.expire_image_notice(Instant::now());
            }
        });
    }

    fn expire_image_notice(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        if state.image_notices.expire(now) {
            self.redraw();
        }
    }

    fn expire_success(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        if state.success_until.is_none_or(|until| now < until) {
            return;
        }
        state.success_until = None;
        state.completed = None;
        state.status = None;
        state.result_token = None;
        drop(state);
        self.redraw();
    }

    fn redraw(&self) {
        let _ = send(&self.writer, &ClientMsg::Repaint);
    }

    pub fn message(self: &Arc<Self>, msg: ServerMsg) {
        let mut state = self.state.lock().unwrap();
        let repaint = matches!(&msg, ServerMsg::AttachmentResult { .. });
        let mut success = false;
        let image = matches!(&msg, ServerMsg::ClipboardImageOffer { .. });
        match msg {
            ServerMsg::ClipboardFocus { pane, agent } => {
                state.focus = Some((pane, agent));
                return;
            }
            ServerMsg::AttachmentPrepared { token, target, limits, broadcast } => {
                if state.uncertain.as_ref() == Some(&token) {
                    return;
                }
                let Some(dropped) = state.dropped.as_mut().filter(|d| d.token == token && !d.ready && !d.result) else {
                    return;
                };
                dropped.target = target.chars().filter(|c| !c.is_control()).collect();
                dropped.limits = limits;
                if dropped.image {
                    dropped.limits.file_bytes = dropped.limits.file_bytes.min(attachments::IMAGE_BYTES as u64);
                    dropped.limits.files = 1;
                }
                dropped.broadcast = broadcast;
                dropped.ready = true;
                dropped.error = dropped.validate().err().map(|e| format!("{e:#}"));
                let image = dropped.image;
                let shortcut = dropped.shortcut;
                let automatic = !dropped.retry
                    && dropped.selected == 0
                    && dropped.error.is_none()
                    && !broadcast
                    && if image {
                        shortcut
                            && (state.image_approved
                                || self
                                    .destination
                                    .as_ref()
                                    .is_some_and(|(ssh, _)| paste::image_allowed(&paste::permission_path(), ssh)))
                    } else {
                        self.destination.as_ref().is_some_and(|(ssh, _)| paste::allowed(&paste::permission_path(), ssh))
                    };
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
                state.success_until = None;
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
                return;
            }
            ServerMsg::AttachmentResult { token, mut message, pasted, mut kind } => {
                let active = state.active.as_ref().is_some_and(|a| a.token == token);
                let prompt = state.prompt.as_ref().is_some_and(|p| p.token == token);
                let uncertain = state.uncertain.as_ref() == Some(&token);
                let dropped = state.dropped.as_ref().is_some_and(|d| d.token == token);
                if !active && !prompt && !uncertain && !dropped {
                    return;
                }
                let completed = state
                    .active
                    .as_ref()
                    .filter(|a| a.token == token)
                    .and_then(|a| a.transfer(crate::theme::current(), Instant::now(), true));
                if active && let Some(active) = state.active.take() {
                    active.cancelled.store(true, Ordering::SeqCst);
                    if let Some(failure) = active.failure {
                        message = failure;
                        kind = NoticeKind::Error;
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
                    if !message.is_empty() {
                        let text = message.starts_with("Text submitted");
                        let summary = if text {
                            "Text pasted".into()
                        } else if !self.remote && !drop.image {
                            "Paths pasted".into()
                        } else {
                            format!("Sent to {}", self.host())
                        };
                        let destination = if message.contains("Focus changed") {
                            format!("Focus changed. Select {} before typing again.", pane_name(&drop.target))
                        } else {
                            format!(
                                "{} pasted into {}.",
                                if text {
                                    "Text"
                                } else if drop.image || drop.paths.len() == 1 {
                                    "Path"
                                } else {
                                    "Paths"
                                },
                                pane_name(&drop.target)
                            )
                        };
                        message = format!("{summary}\n{destination}");
                    }
                } else if dropped {
                    let drop = state.dropped.as_mut().unwrap();
                    if let Some(error) = &drop.error {
                        message = format!("Could not send: {error}. Choose Retry to try again.");
                        kind = NoticeKind::Error;
                    }
                    drop.result = true;
                    drop.selected = if drop.image { 1 } else { 2 };
                }
                if state.active.is_none() && state.prompt.is_none() {
                    if pasted {
                        // A successful paste may have discarded queued input after a focus change.
                        // Release the server's input hold before forwarding any new local keys.
                        let _ = send(&self.writer, &ClientMsg::AttachmentDismiss { token: token.clone() });
                        state.success_until = Some(Instant::now() + SUCCESS_DURATION);
                        state.completed = completed;
                        state.parser = InputParser::new();
                        success = true;
                    } else {
                        state.success_until = None;
                    }
                    state.result_token = Some(token);
                    state.status_kind = kind;
                    state.status = (!message.is_empty()).then(|| friendly_result(&message, dropped));
                }
            }
            _ => {}
        }
        if repaint {
            state.drawn_area = None;
        }
        drop(state);
        if repaint {
            self.redraw();
        }
        self.draw();
        if success {
            let this = Arc::downgrade(self);
            let _ = crate::threads::named("attachment-notice").spawn(move || {
                std::thread::sleep(SUCCESS_DURATION);
                if let Some(this) = this.upgrade() {
                    this.expire_success(Instant::now());
                }
            });
        }
    }

    /// True when the local form consumed the input. Normal typing stays on the same wire.
    pub fn keys(self: &Arc<Self>, bytes: &[u8]) -> bool {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        self.keys_with_screen(bytes, Rect::new(0, 0, cols, rows.saturating_sub(1)))
    }

    fn keys_with_screen(self: &Arc<Self>, bytes: &[u8], screen: Rect) -> bool {
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
                        if mouse.kind == crate::input::MouseKind::Press(crate::input::MouseButton::Left) {
                            action = self.button_at(&state, screen, mouse.col, mouse.row);
                            action = action.map(|index| state.dropped.as_ref().unwrap().action(index));
                        }
                        if action.is_some() {
                            tail = &bytes[(index + 1).min(bytes.len())..];
                            break 'input;
                        }
                        continue;
                    }
                    let InputEvent::Key(key) = event else { continue };
                    if matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown) {
                        let layout = state.view(&self.host()).unwrap().layout(screen);
                        let dropped = state.dropped.as_mut().unwrap();
                        let step = if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                            layout.file_rows.max(1)
                        } else {
                            1
                        };
                        dropped.scroll = dropped.scroll.min(layout.max_scroll);
                        dropped.scroll = if matches!(key.code, KeyCode::Up | KeyCode::PageUp) {
                            dropped.scroll.saturating_sub(step)
                        } else {
                            dropped.scroll.saturating_add(step).min(layout.max_scroll)
                        };
                        continue;
                    }
                    let dropped = state.dropped.as_mut().unwrap();
                    let count = if dropped.image && dropped.shortcut && !dropped.result {
                        3
                    } else if dropped.image {
                        2
                    } else if dropped.result {
                        3
                    } else {
                        4
                    };
                    match key.code {
                        KeyCode::Char('c') if key.mods.ctrl => action = Some(if dropped.result { 2 } else { 3 }),
                        KeyCode::Esc => action = Some(if dropped.result { 2 } else { 3 }),
                        KeyCode::Tab | KeyCode::Right => dropped.selected = (dropped.selected + 1) % count,
                        KeyCode::BackTab | KeyCode::Left => dropped.selected = (dropped.selected + count - 1) % count,
                        KeyCode::Enter if dropped.ready || dropped.result => {
                            action = Some(dropped.action(dropped.selected))
                        }
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
            if state.success_until.is_some() {
                return false;
            }
            let clickable = state.active.is_some() || state.status.is_some();
            for index in 0..bytes.len().max(1) {
                let events =
                    if bytes.is_empty() { state.parser.flush() } else { state.parser.feed(&bytes[index..=index]) };
                if clickable
                    && events.iter().any(|event| match event {
                        InputEvent::Key(key) => {
                            (key.code == KeyCode::Char('c') && key.mods.ctrl)
                                || (state.active.is_none() && matches!(key.code, KeyCode::Enter | KeyCode::Esc))
                        }
                        InputEvent::Mouse(mouse) => {
                            mouse.kind == crate::input::MouseKind::Press(crate::input::MouseButton::Left)
                                && self.button_at(&state, screen, mouse.col, mouse.row).is_some()
                        }
                        _ => false,
                    })
                {
                    drop(state);
                    self.cancel();
                    let tail = &bytes[(index + 1).min(bytes.len())..];
                    if !tail.is_empty() && !self.keys(tail) {
                        self.forward(tail);
                    }
                    return true;
                }
            }
            return false;
        }
        let clicked = state.prompt.as_ref().map(|_| state.view(&self.host()).unwrap().render(screen).1);
        let Some(prompt) = state.prompt.as_mut() else { return false };
        let mut submit = false;
        let mut tail = &bytes[bytes.len()..];
        'input: for index in 0..bytes.len().max(1) {
            let events =
                if bytes.is_empty() { prompt.parser.flush() } else { prompt.parser.feed(&bytes[index..=index]) };
            for event in events {
                match event {
                    InputEvent::Mouse(mouse)
                        if mouse.kind == crate::input::MouseKind::Press(crate::input::MouseButton::Left) =>
                    {
                        match clicked.as_ref().and_then(|buttons| {
                            buttons.iter().position(|area| area.contains((mouse.col, mouse.row).into()))
                        }) {
                            Some(0) if prompt.error.is_none() => {
                                submit = true;
                                tail = &bytes[(index + 1).min(bytes.len())..];
                                break 'input;
                            }
                            Some(1) => {
                                drop(state);
                                self.cancel();
                                return true;
                            }
                            _ => {}
                        }
                    }
                    InputEvent::Key(key)
                        if prompt.image
                            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right) =>
                    {
                        prompt.cursor = 1 - prompt.cursor;
                    }
                    InputEvent::Key(key)
                        if key.code == KeyCode::Esc
                            || (prompt.image && prompt.cursor == 1 && key.code == KeyCode::Enter) =>
                    {
                        drop(state);
                        self.cancel();
                        return true;
                    }
                    InputEvent::Key(key) if key.code == KeyCode::Enter => {
                        if prompt.error.is_none() {
                            submit = true;
                            tail = &bytes[(index + 1).min(bytes.len())..];
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
                        prompt.error =
                            crate::server::paste_line(&mut prompt.text, &mut prompt.cursor, &c.to_string()).err();
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
        if submit
            && !prompt.image
            && let Err(error) = explicit_paths(&prompt.text)
        {
            prompt.error = Some(format!("{error:#}. Edit the paths and try again."));
            submit = false;
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
        if state.dropped.is_none() {
            state.dropped = Some(Dropped {
                image: prompt.image,
                shortcut: false,
                token: prompt.token.clone(),
                original: prompt.text.as_bytes().to_vec(),
                paths: if prompt.image { Vec::new() } else { explicit_paths(&prompt.text).unwrap_or_default() },
                target: prompt.target.clone(),
                limits: prompt.limits.clone(),
                broadcast: false,
                ready: true,
                result: false,
                retry: false,
                selected: 0,
                scroll: 0,
                error: None,
            });
        }
        state.success_until = None;
        state.active = Some(Active {
            token: prompt.token.clone(),
            cancelled: cancelled.clone(),
            ack,
            started: Instant::now(),
            last_draw: None,
            label: String::new(),
            progress: None,
            failure: None,
        });
        state.status = None;
        state.displayed = None;
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
                kind: NoticeKind::Error,
                pasted: false,
                token,
                message: format!("Cannot start attachment: {error}. Use {action} to retry."),
            });
        }
    }

    fn upload_failed(self: &Arc<Self>, token: String, cancelled: &AtomicBool, error: &anyhow::Error, image: bool) {
        let message = if image {
            format!("Image not sent: {error:#}. Choose Retry to try again.")
        } else {
            format!("Files not sent: {error:#}. Choose Retry to try again.")
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
            self.message(ServerMsg::AttachmentResult { kind: NoticeKind::Error, pasted: false, token, message });
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
            state.success_until = None;
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
            let image = dropped.image;
            state.dropped = Some(dropped);
            drop(state);
            let _ = send(&self.writer, &ClientMsg::AttachmentRequest { token, text, image, pane: None });
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
            && let Err(error) = if dropped.image {
                paste::remember_image(&paste::permission_path(), ssh, label)
            } else {
                paste::remember(&paste::permission_path(), ssh, label)
            }
        {
            dropped.error = Some(format!("Cannot remember this server: {error}. Choose Send to send once."));
            state.dropped = Some(dropped);
            drop(state);
            self.draw();
            return;
        }
        if dropped.shortcut {
            state.image_approved = true;
        }
        let prompt = Prompt {
            image: dropped.image,
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
        state.status_kind = NoticeKind::Notice;
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
                std::thread::sleep(PROGRESS_INTERVAL);
                let Some(this) = this.upgrade() else { return };
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                if !this.draw_progress_with(&token, Instant::now(), cols, rows, |bytes| {
                    let _ = this.output_local(bytes);
                }) {
                    return;
                }
            }
        });
    }

    fn draw_progress_with(&self, token: &str, now: Instant, cols: u16, rows: u16, write: impl FnOnce(&[u8])) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(active) = state.active.as_mut().filter(|active| active.token == token) else { return false };
        if now.saturating_duration_since(active.last_draw.unwrap_or(active.started)) < PROGRESS_INTERVAL {
            return true;
        }
        active.last_draw = Some(now);
        let status = active.status(crate::theme::current(), now);
        state.displayed = active.transfer(crate::theme::current(), now, false);
        state.status = Some(status);
        state.status_kind = NoticeKind::Notice;
        if let Some(view) = state.view(&self.host()) {
            let (buf, _) = view.render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
            write(&self.overlay(&mut state, &buf));
        }
        true
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
        {
            let mut state = self.state.lock().unwrap();
            if let Some(active) = state.active.as_mut().filter(|active| active.token == *token) {
                active.label = if specs.len() == 1 { specs[0].name.clone() } else { format!("{} files", specs.len()) };
            }
        }
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
        state.connected = false;
        state.image_notices.clear();
        state.focus = None;
        state.image_keys = Default::default();
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
        state.success_until = None;
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
    fn action(&self, index: usize) -> usize {
        if self.image && self.result && index == 1 {
            2
        } else if self.image && !self.result && index == if self.shortcut { 2 } else { 1 } {
            3
        } else {
            index
        }
    }

    fn validate(&self) -> Result<u64> {
        if self.image {
            return Ok(0);
        }
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
}

pub(super) struct View {
    pub(super) title: String,
    pub(super) kind: NoticeKind,
    pub(super) error_start: Option<usize>,
    pub(super) message: String,
    pub(super) transfer: Option<TransferProgress>,
    pub(super) files: String,
    pub(super) scroll: usize,
    pub(super) buttons: Vec<String>,
    pub(super) selected: usize,
    pub(super) keys: Vec<(&'static str, &'static str)>,
}

impl State {
    fn view(&self, host: &str) -> Option<View> {
        let mut view = View {
            title: String::new(),
            kind: NoticeKind::Notice,
            error_start: None,
            message: String::new(),
            transfer: None,
            files: String::new(),
            scroll: 0,
            buttons: Vec::new(),
            selected: 0,
            keys: Vec::new(),
        };
        if self.active.is_some() {
            view.transfer = self.displayed.clone();
            view.title = format!("Sending to {host}");
            view.message = self.status.clone()?;
            view.buttons = vec!["Cancel".into()];
            view.keys = vec![("Ctrl+C", "Cancel")];
        } else if self.uncertain.is_some() {
            view.title = "Finishing attachment".into();
            view.message = self.status.clone()?;
        } else if let Some(d) = &self.dropped {
            view.selected = d.selected;
            if d.result {
                view.title = "Attachment failed".into();
                view.kind = if d.error.is_some() { NoticeKind::Error } else { self.status_kind };
                view.message =
                    self.status.clone().unwrap_or_else(|| "Transfer interrupted. Choose Retry to try again.".into());
                view.buttons = if d.image {
                    vec!["Retry".into(), "Close".into()]
                } else {
                    vec!["Retry".into(), "Paste as text".into(), "Close".into()]
                };
                view.selected = view.selected.min(view.buttons.len() - 1);
                view.keys = vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Close")];
            } else if !d.ready {
                view.title = "Preparing attachment".into();
                view.message = "Waiting for the destination pane.".into();
                view.buttons = vec!["Cancel".into()];
                view.keys = vec![("Ctrl+C", "Cancel")];
            } else {
                view.title = if d.image {
                    format!("Send the clipboard image to {host}?")
                } else {
                    format!("Send {} {} to {host}?", d.paths.len(), if d.paths.len() == 1 { "file" } else { "files" })
                };
                view.files = d
                    .paths
                    .iter()
                    .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("\n");
                view.scroll = d.scroll;
                view.message = if d.image {
                    format!("Its path on {host} goes to {}.", pane_name(&d.target))
                } else {
                    format!(
                        "{:.1} MiB. Their paths on {host} go to {}.",
                        d.validate().unwrap_or(0) as f64 / 1048576.0,
                        pane_name(&d.target)
                    )
                };
                if d.broadcast {
                    view.message.push_str("\nBroadcasting is on. Paths go only to this pane.");
                }
                if let Some(error) = &d.error {
                    view.error_start = Some(view.message.len());
                    view.message.push_str(&format!("\n{error}"));
                }
                view.buttons = if d.image && d.shortcut {
                    vec!["Send".into(), format!("Always for {}", crate::text::truncate(host, 24)), "Cancel".into()]
                } else if d.image {
                    vec!["Send".into(), "Cancel".into()]
                } else {
                    vec![
                        "Send".into(),
                        format!("Always for {}", crate::text::truncate(host, 24)),
                        "Paste as text".into(),
                        "Cancel".into(),
                    ]
                };
                view.keys = vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Cancel")];
            }
        } else if let Some(p) = &self.prompt {
            view.title = if p.image {
                format!("Send the clipboard image to {host}?")
            } else {
                format!("Attach files to {host}")
            };
            view.message = if p.image {
                format!("Its path on {host} goes to {}.\nSend reads the local clipboard once.", pane_name(&p.target))
            } else {
                format!(
                    "Enter absolute local paths, quoting spaces.\nPaths: {}\nTheir paths on {host} go to {}.",
                    crate::text::truncate_start(&p.text.chars().take(p.cursor).collect::<String>(), 64),
                    pane_name(&p.target)
                )
            };
            if let Some(error) = &p.error {
                view.error_start = Some(view.message.len());
                view.message.push_str(&format!("\n{error}"));
            }
            view.buttons = vec!["Send".into(), "Cancel".into()];
            view.selected = if p.image { p.cursor } else { 0 };
            view.keys = if p.image {
                vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Cancel")]
            } else {
                vec![("Enter", "Send"), ("Ctrl+U", "Clear"), ("Esc", "Cancel")]
            };
        } else if let Some(status) = &self.status {
            view.kind = self.status_kind;
            view.title = if self.success_until.is_some() {
                if self.completed.is_some() { format!("Sent to {host}") } else { "Attachment complete".into() }
            } else {
                "Attachment".into()
            };
            if self.success_until.is_some() {
                view.transfer = self.completed.clone();
                view.message =
                    if let Some((title, body)) = status.split_once('\n').filter(|(_, body)| !body.trim().is_empty()) {
                        view.title = title.into();
                        body.into()
                    } else {
                        status.clone()
                    };
            }
            if self.success_until.is_none() {
                view.message = status.clone();
                view.buttons = vec!["Close".into()];
                view.keys = vec![("Enter", "Close"), ("Esc", "Close")];
            }
        } else {
            return None;
        }
        Some(view)
    }
}

fn friendly_result(message: &str, retry: bool) -> String {
    let message = message
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect::<String>()
        .replace("the reserved pane", "the destination pane");
    if !retry {
        return message;
    }
    message
        .replace("Press Ctrl+C, then use Attach clipboard image to retry.", "Choose Retry to read the clipboard again.")
        .replace("Press Ctrl+C, then use Attach files to retry.", "Choose Retry to send again.")
        .replace(
            "Close this result, then use Attach clipboard image to retry.",
            "Choose Retry to read the clipboard again.",
        )
        .replace("Close this result, then use Attach files to retry.", "Choose Retry to send again.")
}

fn pane_name(target: &str) -> &str {
    target.rsplit(" › ").next().unwrap_or(target).rsplit(" / ").next().unwrap_or(target)
}

impl View {
    pub(super) fn scroll_limits(&self, screen: Rect) -> (usize, usize) {
        let layout = self.layout(screen);
        (layout.max_scroll, layout.file_rows.max(1))
    }

    fn layout(&self, screen: Rect) -> ViewLayout {
        let width = screen.width.saturating_sub(4).min(100);
        let text_width = width.saturating_sub(4) as usize;
        let content = if let Some(transfer) = &self.transfer {
            format!("{}\n{}", transfer.message(text_width), self.message)
        } else {
            self.message.clone()
        };
        let message = crate::text::wrap(&content, text_width);
        let files = self.files.lines().flat_map(|line| crate::text::fold(line, text_width)).collect::<Vec<_>>();
        let mut button_rows = 0;
        let mut used = 0;
        for label in &self.buttons {
            let width = crate::text::columns(label).min(text_width.saturating_sub(2)) + 2;
            if button_rows == 0 || used + width > text_width {
                button_rows += 1;
                used = 0;
            }
            used += width + 3;
        }
        let base = if button_rows == 0 { 2 } else { 4 + button_rows };
        let height = (message.len() + files.len() + base).min(screen.height.saturating_sub(2) as usize) as u16;
        let area = overlays::centered(screen, width, height);
        let body = area.height.saturating_sub(base as u16) as usize;
        let mut file_rows = body.saturating_sub(message.len());
        if files.len() > file_rows {
            file_rows = file_rows.saturating_sub(1);
        }
        let max_scroll = files.len().saturating_sub(file_rows);
        let error_start = self.error_start.map(|start| crate::text::wrap(&self.message[..start], text_width).len());
        ViewLayout { area, message, error_start, files, file_rows, max_scroll, body, button_rows: button_rows as u16 }
    }

    pub(super) fn render(&self, screen: Rect) -> (Buffer, Vec<Rect>) {
        let layout = self.layout(screen);
        let area = layout.area;
        let mut buf = Buffer::empty(area);
        let theme = crate::theme::current();
        let title_colour = if self.buttons.is_empty() { self.kind.colour(theme) } else { theme.accent };
        let inner = overlays::frame(area, &self.title, title_colour, &mut buf);
        let mut buttons = Vec::new();
        if inner.width < 3 || inner.height == 0 {
            return (buf, buttons);
        }
        if !self.buttons.is_empty() && inner.height < 4 {
            return (buf, buttons);
        }
        let width = inner.width.saturating_sub(2);
        let x = inner.x + 1;
        let mut bx = x;
        let mut by = inner.bottom().saturating_sub(1 + layout.button_rows);
        for (index, label) in self.buttons.iter().enumerate() {
            let label = crate::text::truncate(label, width.saturating_sub(2) as usize);
            let w = crate::text::columns(&label) as u16 + 2;
            if bx + w > x + width {
                bx = x;
                by += 1;
            }
            if by >= inner.bottom() - 1 {
                break;
            }
            let rect = Rect::new(bx, by, w, 1);
            buf.set_stringn(
                bx,
                by,
                format!(" {label} "),
                w as usize,
                overlays::button_style(index == self.selected, false),
            );
            buttons.push(rect);
            bx += w + 3;
        }
        let body = layout.body;
        let scroll = self.scroll.min(layout.max_scroll);
        for (row, line) in
            layout.message.iter().chain(layout.files.iter().skip(scroll).take(layout.file_rows)).take(body).enumerate()
        {
            let kind = if row >= layout.message.len() {
                NoticeKind::Notice
            } else if layout.error_start.is_some_and(|start| row >= start) {
                NoticeKind::Error
            } else {
                self.kind
            };
            buf.set_stringn(x, inner.y + row as u16, line, width as usize, Style::default().fg(kind.colour(theme)));
        }
        if layout.max_scroll > 0 && layout.message.len() < body {
            buf.set_stringn(
                x,
                inner.y + body as u16 - 1,
                format!("PgUp/PgDn: filenames ({}/{})", scroll + 1, layout.max_scroll + 1),
                width as usize,
                Style::default().fg(crate::theme::current().text),
            );
        }
        let keys: Vec<_> = self
            .keys
            .iter()
            .map(|&(key, what)| {
                if key == "Enter" && what == "Choose" {
                    (key, self.buttons.get(self.selected).map_or(what, String::as_str))
                } else {
                    (key, what)
                }
            })
            .collect();
        if !keys.is_empty() {
            buf.set_line(x, inner.bottom() - 1, &overlays::key_line(&keys, "", width), width);
        }
        (buf, buttons)
    }
}

struct ViewLayout {
    area: Rect,
    message: Vec<String>,
    error_start: Option<usize>,
    files: Vec<String>,
    file_rows: usize,
    max_scroll: usize,
    body: usize,
    button_rows: u16,
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

    struct Size(usize, usize);
    impl alacritty_terminal::grid::Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    #[test]
    fn attachment_results_keep_their_kind_and_draw_it_locally_and_over_ssh() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
        for remote in [false, true] {
            let (client, _peer) = controller(remote);
            for (kind, message, colour) in [
                (NoticeKind::Notice, "Files received.", NamedColor::White),
                (NoticeKind::Warning, "Focus changed: queued typing discarded.", NamedColor::BrightRed),
                (NoticeKind::Error, "Transfer interrupted.", NamedColor::BrightRed),
            ] {
                client.state.lock().unwrap().uncertain = Some("ab".into());
                client.message(ServerMsg::AttachmentResult {
                    kind,
                    token: "ab".into(),
                    message: message.into(),
                    pasted: kind != NoticeKind::Error,
                });
                assert_eq!(client.state.lock().unwrap().status_kind, kind);
                for (cols, rows) in [(80, 24), (124, 36)] {
                    let mut bytes = Vec::new();
                    client.draw_with(cols, rows, |out| bytes.extend_from_slice(out));
                    let mut terminal = Term::new(Config::default(), &Size(cols as usize, rows as usize), VoidListener);
                    let mut parser: Processor = Processor::new();
                    parser.advance(&mut terminal, &bytes);
                    let mut found = false;
                    for row in 0..rows {
                        let line = Line(row as i32);
                        let text: String = (0..cols).map(|col| terminal.grid()[line][Column(col as usize)].c).collect();
                        if let Some(start) = text.find(message) {
                            found = true;
                            let start = text[..start].chars().count();
                            for col in start..start + message.chars().count() {
                                assert_eq!(terminal.grid()[line][Column(col)].fg, Color::Named(colour));
                            }
                        }
                    }
                    assert!(found, "missing result in attachment dialog: {message}");
                }
            }
        }
    }

    fn progress_client() -> (Arc<Controller>, Reader, Instant) {
        let (client, peer) = controller(true);
        let started = Instant::now();
        client.state.lock().unwrap().active = Some(Active {
            token: "ab".into(),
            cancelled: Arc::new(AtomicBool::new(false)),
            ack: bounded(2).0,
            started,
            last_draw: None,
            label: "capture.png".into(),
            progress: Some((0, 4 * 1048576)),
            failure: None,
        });
        (client, peer, started)
    }

    #[test]
    fn progress_content_at_124_and_80_columns_in_each_theme() {
        for theme in [crate::theme::Theme::DEFAULT, crate::theme::Theme::ASCII] {
            for cols in [124, 80] {
                for percent in [0, 50, 100] {
                    for label in ["capture.png", "2 files"] {
                        let (client, _peer, started) = progress_client();
                        let mut state = client.state.lock().unwrap();
                        let active = state.active.as_mut().unwrap();
                        active.label = label.into();
                        active.progress = Some((4 * 1048576 * percent / 100, 4 * 1048576));
                        state.status = Some(active.status(theme, started + PROGRESS_INTERVAL));
                        let mut view = state.view("SERVER").unwrap();
                        view.transfer =
                            state.active.as_ref().unwrap().transfer(theme, started + PROGRESS_INTERVAL, false);
                        let (buf, buttons) = view.render(Rect::new(0, 0, cols, 35));
                        assert_eq!(buf.area.height, 9);
                        let bar_width = buf.area.width as usize - 6;
                        let filled = (percent as usize * bar_width + 50) / 100;
                        let (on, off) = if theme.ascii { ("#", "-") } else { ("▓", "░") };
                        let text = buf
                            .content
                            .chunks(buf.area.width as usize)
                            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                            .collect::<Vec<_>>()
                            .join("\n");
                        for expected in [
                            "Sending to SERVER".to_string(),
                            label.into(),
                            format!("{} / 4.0 MiB ({percent}%)", crate::text::human_size(4 * 1048576 * percent / 100)),
                            format!(
                                "{} {}{}",
                                if theme.ascii { "/" } else { "⠙" },
                                on.repeat(filled),
                                off.repeat(bar_width - filled)
                            ),
                            "Typing waits until it finishes.".into(),
                            "Ctrl+C Cancel".into(),
                        ] {
                            assert!(text.contains(&expected), "missing {expected:?} at {cols} columns:\n{text}");
                        }
                        assert_eq!(buttons.len(), 1);
                        if theme.ascii {
                            assert!(state.status.as_ref().unwrap().is_ascii());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn progress_timer_bounds_redraws_and_animates_without_acknowledgements() {
        let (client, peer, started) = progress_client();
        let mut frames = Vec::new();
        for millis in 0..=1000 {
            client.message(ServerMsg::AttachmentAck { token: "ab".into(), received: millis });
            assert!(client.draw_progress_with("ab", started + Duration::from_millis(millis), 80, 24, |bytes| {
                frames.push((millis, bytes.to_vec()));
            }));
        }
        assert_eq!(frames.iter().map(|(millis, _)| *millis).collect::<Vec<_>>(), [200, 400, 600, 800, 1000]);
        assert!(frames.windows(2).all(|pair| pair[0].1 != pair[1].1));
        assert!(peer.incoming.try_recv().is_err());
        let mut stalled = Vec::new();
        client.draw_progress_with("ab", started + Duration::from_millis(1200), 80, 24, |bytes| {
            stalled.extend_from_slice(bytes);
        });
        assert_ne!(frames.last().unwrap().1, stalled);
        assert!(String::from_utf8(stalled).unwrap().contains("0 B / 4.0 MiB (0%)"));
    }

    #[test]
    fn incoming_output_reuses_progress_until_the_next_tick() {
        let (client, _peer, started) = progress_client();
        client.state.lock().unwrap().active.as_mut().unwrap().progress = None;
        for millis in [200, 400] {
            let mut displayed = Vec::new();
            client.draw_progress_with("ab", started + Duration::from_millis(millis), 80, 24, |out| {
                displayed.extend_from_slice(out);
            });
            client.progress("ab", millis * 1024, 4 * 1048576);
            client.draw_progress_with("ab", started + Duration::from_millis(millis + 199), 80, 24, |_| {
                panic!("progress must wait for its next tick");
            });
            client
                .output_with(b"\x1b[1;1Hpane output", 80, 24, |out| {
                    let mut expected = b"\x1b[?2026h\x1b[1;1Hpane output".to_vec();
                    expected.extend_from_slice(&displayed);
                    expected.extend_from_slice(b"\x1b[?2026l");
                    assert_eq!(out, expected, "incoming output must reuse the entire displayed snapshot");
                    Ok(true)
                })
                .unwrap();
        }
        client.draw_progress_with("ab", started + Duration::from_millis(600), 80, 24, |out| {
            assert!(String::from_utf8_lossy(out).contains("400 KiB / 4.0 MiB (9%)"));
        });
    }

    #[test]
    fn clearing_a_validation_error_repaints_uncovered_pane_content() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;

        let (client, mut peer) = controller(false);
        let mut terminal = Term::new(Config::default(), &Size(80, 24), VoidListener);
        let mut parser: Processor = Processor::new();
        let background = (1..=24).map(|row| format!("\x1b[{row};1H{}", "p".repeat(80))).collect::<String>();
        parser.advance(&mut terminal, background.as_bytes());
        client.message(ServerMsg::AttachmentOffer {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
        });
        assert!(client.keys(b"\r"));
        client.draw_with(80, 24, |out| parser.advance(&mut terminal, out));
        let previous = client.state.lock().unwrap().drawn_area.unwrap();
        assert_eq!(previous.height, 10);
        assert_eq!(terminal.grid()[Line(previous.y as i32)][Column(previous.x as usize)].c, '╭');
        assert!(client.keys(b"\x15"));
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        client
            .output_with(background.as_bytes(), 80, 24, |out| {
                parser.advance(&mut terminal, out);
                Ok(true)
            })
            .unwrap();
        let current = client.state.lock().unwrap().drawn_area.unwrap();
        assert_eq!(current.height, 8);
        for row in previous.y..previous.bottom() {
            for col in previous.x..previous.right() {
                if !current.contains((col, row).into()) {
                    assert_eq!(terminal.grid()[Line(row as i32)][Column(col as usize)].c, 'p');
                }
            }
        }
        assert_eq!(terminal.grid()[Line(current.y as i32)][Column(current.x as usize)].c, '╭');
        assert!(peer.incoming.try_recv().is_err());
    }

    #[test]
    fn acknowledgements_do_not_wait_for_terminal_redraw() {
        let (client, _peer, started) = progress_client();
        client.draw_progress_with("ab", started + PROGRESS_INTERVAL, 80, 24, |_| {});
        let terminal = super::super::terminal();
        let (finished, done) = bounded(1);
        let worker = std::thread::spawn(move || {
            for received in 0..1000 {
                client.message(ServerMsg::AttachmentAck { token: "ab".into(), received });
            }
            finished.send(()).unwrap();
        });
        let completed = done.recv_timeout(Duration::from_secs(2)).is_ok();
        drop(terminal);
        worker.join().unwrap();
        assert!(completed, "acknowledgements must not draw a frame for every chunk");
    }

    #[test]
    fn results_replace_progress_and_short_transfers_never_show_it() {
        for elapsed in [Duration::from_millis(199), PROGRESS_INTERVAL] {
            for pasted in [false, true] {
                let (client, mut peer, started) = progress_client();
                let mut frames = Vec::new();
                client.draw_progress_with("ab", started + elapsed, 80, 24, |bytes| frames.push(bytes.to_vec()));
                assert_eq!(frames.len(), usize::from(elapsed == PROGRESS_INTERVAL));
                client.progress("ab", 8 * 1048576, 8 * 1048576);
                let result = if pasted { "Files received." } else { "Transfer interrupted." };
                client.message(ServerMsg::AttachmentResult {
                    token: "ab".into(),
                    kind: if pasted { NoticeKind::Notice } else { NoticeKind::Error },
                    message: result.into(),
                    pasted,
                });
                if pasted {
                    assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                }
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                client.draw_with(80, 24, |bytes| {
                    let text = String::from_utf8_lossy(bytes);
                    assert!(text.contains(result));
                    if pasted {
                        assert!(text.contains("8.0 MiB / 8.0 MiB (100%)"));
                    }
                    assert!(!text.contains("Sending to") && !text.contains("Ctrl+C Cancel"));
                });
                assert!(!client.draw_progress_with("ab", started + Duration::from_secs(1), 80, 24, |_| {
                    panic!("progress must not repaint over a result");
                }));
            }
        }
    }

    #[test]
    fn progress_preparation_sizes_and_filename_are_readable() {
        let (client, _peer, started) = progress_client();
        let mut state = client.state.lock().unwrap();
        let active = state.active.as_mut().unwrap();
        for theme in [crate::theme::Theme::DEFAULT, crate::theme::Theme::ASCII] {
            active.progress = None;
            assert!(active.status(theme, started).contains("Preparing attachment."));
            active.label = format!("capture\n\x1b{}", "界".repeat(80));
            for (received, total, expected) in
                [(0, 0, "0 B / 0 B (100%)"), (0, 3, "0 B / 3 B (0%)"), (512, 1024, "512 B / 1.0 KiB (50%)")]
            {
                active.progress = Some((received, total));
                let status = active.transfer(theme, started, false).unwrap().message(72);
                assert!(status.contains(expected));
                let label = status.lines().next().unwrap();
                assert!(!label.contains('\x1b'));
                assert!(crate::text::columns(label) <= 64);
            }
        }
    }

    #[test]
    fn progress_draw_keeps_state_locked_until_output_finishes() {
        for (kind, message) in [(NoticeKind::Notice, "Files received."), (NoticeKind::Error, "Transfer interrupted.")] {
            let (client, _peer) = controller(false);
            client.state.lock().unwrap().status = Some("Sending".into());
            let frames = Mutex::new(Vec::new());
            let (snapshot_tx, snapshot_rx) = bounded(1);
            let (resume_tx, resume_rx) = bounded(1);
            let (locked_tx, locked_rx) = bounded(1);
            std::thread::scope(|scope| {
                let progress = scope.spawn(|| {
                    client.draw_with(80, 24, |bytes| {
                        snapshot_tx.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                        frames.lock().unwrap().push(String::from_utf8(bytes.to_vec()).unwrap());
                    });
                });
                snapshot_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                let result = scope.spawn(|| {
                    let locked = matches!(client.state.try_lock(), Err(std::sync::TryLockError::WouldBlock));
                    locked_tx.send(locked).unwrap();
                    {
                        let mut state = client.state.lock().unwrap();
                        state.status = Some(message.into());
                        state.status_kind = kind;
                    }
                    client.draw_with(80, 24, |bytes| {
                        frames.lock().unwrap().push(String::from_utf8(bytes.to_vec()).unwrap());
                    });
                });
                let locked = locked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                resume_tx.send(()).unwrap();
                progress.join().unwrap();
                result.join().unwrap();
                assert!(locked, "a result must wait until the progress snapshot is written");
            });
            let frames = frames.lock().unwrap();
            assert_eq!(frames.len(), 2);
            assert!(frames[0].contains("Sending"));
            assert!(frames[1].contains(message));
        }
    }

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
            host: "alice@preparing-test".into(),
            session: "main".into(),
            label: "box".into(),
            pane: None,
            clipboard_native: false,
        };
        let (writer, peer) = wire();
        let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target, None);
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

    fn read_without_repaint(peer: &mut Reader) -> ClientMsg {
        loop {
            match read(peer) {
                ClientMsg::Repaint | ClientMsg::Resize { .. } => {}
                message => return message,
            }
        }
    }

    fn dropped_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("kraal-pasted-{name}-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        path
    }

    fn prepare_drop(client: &Arc<Controller>, peer: &mut Reader, original: &[u8]) -> String {
        client.connected();
        client.input(&paste::bracket(original));
        assert!(matches!(read(peer), ClientMsg::PasteStart));
        let ClientMsg::AttachmentRequest { token, text, image: false, .. } = read(peer) else { panic!() };
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
    fn pending_keys_stay_before_pastes_and_pasted_control_v_never_probes() {
        for text in [b"".as_slice(), b"text\x16\x1b[118;5u"] {
            let (client, mut peer) = controller(true);
            client.connected();
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
            let bytes = [b"\x1b".as_slice(), &paste::bracket(text), b"after"].concat();
            client.input_with_probe(&bytes, || panic!("paste data cannot inspect the clipboard"));
            let mut received = Vec::new();
            while received.len() < bytes.len() {
                let (kind, data) = protocol::read_frame(&mut peer).unwrap().unwrap();
                if kind == protocol::BYTES {
                    received.extend(data);
                } else {
                    assert!(matches!(serde_json::from_slice::<ClientMsg>(&data).unwrap(), ClientMsg::PasteStart));
                }
            }
            assert_eq!(received, bytes);
            assert!(!client.pending_keys());
        }
    }

    #[test]
    fn control_v_only_probes_remote_panes_and_forwards_exact_keys_without_an_image() {
        for remote in [false, true] {
            for agent in [false, true] {
                for connected in [false, true] {
                    for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
                        let (client, mut peer) = controller(remote);
                        if connected {
                            client.connected();
                        }
                        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent });
                        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                        let count = Arc::clone(&probes);
                        let handled = client.input_with_probe(key, move || {
                            count.fetch_add(1, Ordering::SeqCst);
                            Ok(false)
                        });
                        if !handled {
                            client.forward(key);
                        }
                        wait_for_probe(&client);
                        assert_eq!(probes.load(Ordering::SeqCst), usize::from(remote && connected));
                        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, key.to_vec()));
                        assert!(peer.incoming.try_recv().is_err());
                        let state = client.state.lock().unwrap();
                        assert!(state.workers.is_empty());
                        assert_eq!(state.image_notices.render(80, 24).is_empty(), !(remote && agent && connected));
                    }
                }
            }
        }
        let (client, mut peer) = controller(true);
        client.connected();
        client.input_with_probe(b"\x16", || panic!("no focus received"));
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![0x16]));
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        client.disconnect();
        assert!(client.state.lock().unwrap().focus.is_none());
    }

    #[test]
    fn image_diagnostics_forward_keys_once_per_cause_and_survive_reconnection() {
        for key in [b"\x16".as_slice(), b"\x1b[118;5u", b"\x1b[118;133u", b"\x1b[118;5:1;118u"] {
            let (client, mut peer) = controller(true);
            client.connected();
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
            for failure in 0..4 {
                for attempt in 0..2 {
                    client.input_with_probe(key, move || match failure {
                        0 => Ok(false),
                        1 => Err(Diagnostic::MissingTool("wl-clipboard")),
                        2 => Err(Diagnostic::Failed("wl-paste: permission denied".into())),
                        _ => Err(Diagnostic::NoDisplay),
                    });
                    assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, key.to_vec()));
                    let mut drawn = Vec::new();
                    client.draw_with(80, 24, |bytes| drawn.extend_from_slice(bytes));
                    assert_eq!(drawn.is_empty(), attempt != 0);
                    let mut state = client.state.lock().unwrap();
                    assert!(state.view("box").is_none(), "a diagnostic must not open a form");
                    assert!(state.dropped.is_none());
                    state.image_notices.clear();
                }
            }
            client.disconnect();
            client.connected();
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
            client.input_with_probe(key, || Ok(false));
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, key.to_vec()));
            assert!(client.state.lock().unwrap().image_notices.render(80, 24).is_empty());
            client.input_with_probe(b"\r", || panic!("ordinary input must not probe"));
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![b'\r']));
        }
    }

    fn wait_for_probe(client: &Controller) {
        let done = client.state.lock().unwrap().image_probe.clone();
        if let Some(done) = done {
            done.recv_timeout(crate::testing::PATIENCE).unwrap();
        }
    }

    #[test]
    fn a_stalled_non_agent_probe_does_not_hold_control_v_or_following_typing() {
        for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
            let (client, mut peer) = controller(true);
            client.connected();
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: false });
            let (entered, started) = bounded(1);
            let (resume, paused) = bounded(1);
            let input = Arc::clone(&client);
            let bytes = [key, b"same read"].concat();
            let thread = std::thread::spawn(move || {
                input.input_with_probe(&bytes, move || {
                    entered.send(()).unwrap();
                    paused.recv_timeout(crate::testing::PATIENCE).unwrap();
                    Ok(true)
                });
                input.input(b"next read");
            });
            started.recv_timeout(crate::testing::PATIENCE).unwrap();
            let expected = [key, b"same readnext read"].concat();
            let mut received = Vec::new();
            while received.len() < expected.len() {
                let (kind, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
                assert_eq!(kind, protocol::BYTES);
                received.extend(bytes);
            }
            assert_eq!(received, expected);
            assert!(!client.state.lock().unwrap().image_notices.is_active());
            resume.send(()).unwrap();
            thread.join().unwrap();
            wait_for_probe(&client);
            assert!(client.state.lock().unwrap().image_notices.is_active());
        }
    }

    #[test]
    fn copy_mode_control_v_neither_probes_nor_spends_the_unrecognized_notice() {
        let (client, mut peer) = controller(true);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: None, agent: true });
        client.input_with_probe(b"\x16", || panic!("copy mode must not inspect the clipboard"));
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![0x16]));
        assert!(!client.state.lock().unwrap().image_notices.is_active());
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: false });
        client.input_with_probe(b"\x16", || Ok(true));
        wait_for_probe(&client);
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![0x16]));
        assert!(client.state.lock().unwrap().image_notices.is_active());
    }

    #[test]
    fn replacing_a_taller_notice_restores_its_whole_footprint_and_expiry_repaints() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;
        let (client, mut peer) = controller(true);
        let mut terminal = Term::new(Config::default(), &Size(80, 24), VoidListener);
        let mut parser: Processor = Processor::new();
        let background = b"\x1b[2J\x1b[23;1Hpane content on row 23\x1b[24;1Hpane content on row 24";
        parser.advance(&mut terminal, background);
        client.image_notice(Diagnostic::NotAgent);
        client.draw_with(80, 24, |out| parser.advance(&mut terminal, out));
        let row = |terminal: &Term<VoidListener>, row| -> String {
            (0..80).map(|col| terminal.grid()[Line(row)][Column(col)].c).collect::<String>().trim_end().to_owned()
        };
        assert!(row(&terminal, 22).contains("not recognized as an agent"));
        client.image_notice(Diagnostic::NoImage);
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        client
            .output_with(background, 80, 24, |out| {
                parser.advance(&mut terminal, out);
                Ok(true)
            })
            .unwrap();
        assert_eq!(row(&terminal, 22), "pane content on row 23");
        assert!(row(&terminal, 23).contains("No image in the laptop clipboard"));
        client.expire_image_notice(Instant::now() + Duration::from_secs(5));
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        client
            .output_with(background, 80, 24, |out| {
                parser.advance(&mut terminal, out);
                Ok(true)
            })
            .unwrap();
        assert_eq!(row(&terminal, 22), "pane content on row 23");
        assert_eq!(row(&terminal, 23), "pane content on row 24");
        assert!(peer.incoming.try_recv().is_err());
    }

    #[test]
    fn an_unrecognized_pane_only_explains_a_clipboard_that_advertises_an_image() {
        let (client, mut peer) = controller(true);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: false });
        for result in
            [Ok(false), Err(Diagnostic::MissingTool("wl-clipboard")), Err(Diagnostic::Failed("failed".into()))]
        {
            let result = Arc::new(Mutex::new(Some(result)));
            client.input_with_probe(b"\x16", move || result.lock().unwrap().take().unwrap());
            wait_for_probe(&client);
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![0x16]));
            assert!(client.state.lock().unwrap().image_notices.render(80, 24).is_empty());
        }
        for attempt in 0..2 {
            client.input_with_probe(b"\x16", || Ok(true));
            wait_for_probe(&client);
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![0x16]));
            let mut state = client.state.lock().unwrap();
            assert_eq!(state.image_notices.render(124, 24).is_empty(), attempt != 0);
            assert!(state.dropped.is_none());
            state.image_notices.clear();
        }
        assert!(peer.incoming.try_recv().is_err());
    }

    #[test]
    fn delayed_focus_updates_preserve_shortcut_and_coalesced_mode_keys_in_wire_order() {
        for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
            for coalesced in [false, true] {
                let (client, mut peer) = controller(true);
                client.connected();
                client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
                let prefix = b"\x02v";
                if coalesced {
                    client.input_with_probe(&[prefix.as_slice(), key, b"l"].concat(), || Ok(true));
                } else {
                    client.input(prefix);
                    client.input_with_probe(key, || Ok(true));
                    client.input(b"l");
                }
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, prefix.to_vec()));
                let ClientMsg::AttachmentRequest { token, text, image: true, pane: Some(7) } = read(&mut peer) else {
                    panic!("the stale focus offers an image");
                };
                assert_eq!(text, key, "the server can replay the exact key before later input");
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"l".to_vec()));
                // The server has already entered copy mode, but its focus update arrives late.
                client.message(ServerMsg::ClipboardFocus { pane: None, agent: true });
                client.message(ServerMsg::AttachmentResult {
                    token: token.clone(),
                    pasted: true,
                    message: String::new(),
                    kind: NoticeKind::Notice,
                });
                assert!(
                    matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentDismiss { token: sent } if sent == token)
                );
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                let state = client.state.lock().unwrap();
                assert!(state.dropped.is_none());
                assert!(state.view("box").is_none(), "normal input opens no failure dialog");
                assert!(state.workers.is_empty(), "no image was read");
                assert!(peer.incoming.try_recv().is_err());
            }
        }
    }

    #[test]
    fn closing_a_cancelled_image_result_preserves_coalesced_typing() {
        for close in [b"\r".as_slice(), b"\x1b[13u", b"\x03"] {
            let (client, mut peer) = controller(true);
            client.connected();
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
            client.input_with_probe(b"\x16", || Ok(true));
            let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
            client.message(ServerMsg::AttachmentPrepared {
                token: token.clone(),
                target: "Claude".into(),
                limits: Default::default(),
                broadcast: false,
            });
            client.input(b"\t\t\r");
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { token: sent } if sent == token));
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            client.message(ServerMsg::AttachmentResult {
                token: token.clone(),
                pasted: false,
                message: "Attachment stopped: cancelled".into(),
                kind: NoticeKind::Error,
            });
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            assert_eq!(client.state.lock().unwrap().view("box").unwrap().buttons, ["Close"]);
            client.input(&[close, b"cancelled"].concat());
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { token: sent } if sent == token));
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"cancelled".to_vec()));
            assert!(client.state.lock().unwrap().view("box").is_none());
            assert!(peer.incoming.try_recv().is_err());
        }
    }

    #[test]
    fn control_v_question_keeps_the_key_local_and_cancel_reads_nothing() {
        for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
            for split in 1..=key.len() {
                for cancel in [b"\x1b".as_slice(), b"\t\t\r", b"\x03"] {
                    let (client, mut peer) = controller(true);
                    client.connected();
                    client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
                    client.input_with_probe(&key[..split], || Ok(true));
                    if split < key.len() {
                        client.input_with_probe(&key[split..], || Ok(true));
                    }
                    let ClientMsg::AttachmentRequest { token, text, image: true, pane: Some(7) } = read(&mut peer)
                    else {
                        panic!()
                    };
                    assert_eq!(text, key);
                    client.message(ServerMsg::AttachmentPrepared {
                        token: token.clone(),
                        target: "main / Claude".into(),
                        limits: Default::default(),
                        broadcast: false,
                    });
                    {
                        let state = client.state.lock().unwrap();
                        let view = state.view("box").unwrap();
                        assert_eq!(view.title, "Send the clipboard image to box?");
                        assert_eq!(view.buttons, ["Send", "Always for box", "Cancel"]);
                        for width in [80, 124] {
                            assert_eq!(view.render(Rect::new(0, 0, width, 24)).1.len(), 3);
                        }
                        assert!(state.workers.is_empty());
                    }
                    assert!(peer.incoming.try_recv().is_err());
                    client.input(cancel);
                    client.input(&[]);
                    assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { token: sent } if sent == token));
                    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                    assert!(peer.incoming.try_recv().is_err());
                    let state = client.state.lock().unwrap();
                    assert!(!state.image_approved);
                    assert!(state.workers.is_empty());
                }
            }
        }
    }

    #[test]
    fn control_v_send_and_always_share_the_image_transport_without_sending_the_key() {
        if !crate::testing::in_own_process(
            "client::attachments::tests::control_v_send_and_always_share_the_image_transport_without_sending_the_key",
        ) {
            return;
        }
        let root = crate::paths::state_dir().join("shortcut-image-test");
        std::fs::create_dir_all(&root).unwrap();
        let log = root.join("reads");
        #[cfg(not(target_os = "macos"))]
        let (tool, script) = (
            "wl-paste",
            format!(
                "#!/bin/sh\n[ \"$WAYLAND_DISPLAY\" = test ] || exit 7\nif [ \"$1\" = --list-types ]; then printf 'image/png\\ntext/plain\\n'; exit; fi\necho read >> '{}'\nprintf '\\211PNG\\r\\n\\032\\n'\n",
                log.display()
            ),
        );
        #[cfg(target_os = "macos")]
        let (tool, script) = (
            "osascript",
            format!(
                "#!/bin/sh\nif [ \"$2\" = 'clipboard info' ]; then echo '«class PNGf», 8'; exit; fi\necho read >> '{}'\nfor arg; do file=$arg; done\nprintf '\\211PNG\\r\\n\\032\\n' > \"$file\"\n",
                log.display()
            ),
        );
        crate::testing::write_script(&root.join(tool), &script);
        // This isolated test has not started workers yet.
        unsafe {
            std::env::set_var("PATH", format!("{}:{}", root.display(), std::env::var("PATH").unwrap()));
            std::env::set_var("WAYLAND_DISPLAY", "test");
            std::env::remove_var("DISPLAY");
        }
        let make_client = || {
            let target = super::super::Target::Remote {
                ssh: vec!["alice@clipboard-test".into()],
                host: "alice@clipboard-test".into(),
                session: "main".into(),
                label: "box".into(),
                pane: None,
                clipboard_native: false,
            };
            let (writer, peer) = wire();
            let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target, None);
            client.connected();
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
            (client, peer)
        };
        for always in [false, true] {
            let (client, mut peer) = make_client();
            for attempt in 0..2 {
                assert!(client.input(b"\x16"));
                if attempt > 0 {
                    assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                }
                let ClientMsg::AttachmentRequest { token, image: true, .. } = read(&mut peer) else { panic!() };
                client.message(ServerMsg::AttachmentPrepared {
                    token: token.clone(),
                    target: "Claude".into(),
                    limits: Default::default(),
                    broadcast: false,
                });
                if attempt == 0 {
                    assert!(client.state.lock().unwrap().active.is_none());
                    client.input(if always { b"\t\r" } else { b"\r" });
                }
                assert!(client.state.lock().unwrap().active.is_some());
                let ClientMsg::AttachmentStart { files, .. } = read_without_repaint(&mut peer) else { panic!() };
                assert_eq!(files[0].name, "clipboard.png");
                client.message(ServerMsg::AttachmentAck { token: token.clone(), received: 0 });
                let ClientMsg::AttachmentChunk { data, .. } = read_without_repaint(&mut peer) else { panic!() };
                assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), b"\x89PNG\r\n\x1a\n");
                client.message(ServerMsg::AttachmentAck { token: token.clone(), received: 8 });
                assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentFinish { .. }));
                client.message(ServerMsg::AttachmentResult {
                    token,
                    pasted: true,
                    kind: NoticeKind::Notice,
                    message: "Image path pasted.".into(),
                });
                assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            }
            client.shutdown();
            let (client, mut peer) = make_client();
            client.input(b"\x16");
            let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
            client.message(ServerMsg::AttachmentPrepared {
                token,
                target: "Codex".into(),
                limits: Default::default(),
                broadcast: false,
            });
            assert_eq!(client.state.lock().unwrap().active.is_some(), always);
            client.shutdown();
        }
        assert!(std::fs::read_to_string(log).unwrap().lines().count() >= 4);
    }

    #[test]
    fn bare_escape_cancels_initial_prompts_on_idle_input() {
        for remote in [false, true] {
            for image in [false, true] {
                let (client, mut peer) = controller(remote);
                client.connected();
                let token = "ab".to_string();
                client.message(if image {
                    ServerMsg::ClipboardImageOffer {
                        token: token.clone(),
                        target: "pane".into(),
                        limits: Default::default(),
                    }
                } else {
                    ServerMsg::AttachmentOffer {
                        token: token.clone(),
                        target: "pane".into(),
                        limits: Default::default(),
                    }
                });
                assert!(client.input(b"\x1b"));
                assert!(client.pending_keys());
                assert!(client.state.lock().unwrap().prompt.is_some());
                assert!(client.input(&[]));
                assert!(!client.pending_keys());
                assert!(client.state.lock().unwrap().prompt.is_none());
                assert!(
                    matches!(read(&mut peer), ClientMsg::AttachmentCancel { token: cancelled } if cancelled == token)
                );
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                assert!(client.state.lock().unwrap().active.is_none());
            }
        }
    }

    #[test]
    fn expired_initial_prompts_explain_close_and_reopen_without_retry() {
        for image in [false, true] {
            let (client, mut peer) = controller(true);
            let token = "ab".to_string();
            client.message(if image {
                ServerMsg::ClipboardImageOffer {
                    token: token.clone(),
                    target: "pane".into(),
                    limits: Default::default(),
                }
            } else {
                ServerMsg::AttachmentOffer { token: token.clone(), target: "pane".into(), limits: Default::default() }
            });
            let action = if image { "Attach clipboard image" } else { "Attach files" };
            client.message(ServerMsg::AttachmentResult {
                kind: NoticeKind::Error,
                token: token.clone(),
                message: format!("Attachment stopped: the reserved pane closed or the attachment timed out. No paths or queued typing pasted. Close this result, then use {action} to retry."),
                pasted: false,
            });
            {
                let state = client.state.lock().unwrap();
                assert!(state.prompt.is_none());
                assert!(state.dropped.is_none());
                let view = state.view("host").unwrap();
                assert_eq!(view.buttons, ["Close"]);
                assert!(view.message.contains(&format!("Close this result, then use {action} to retry.")));
                assert!(!view.message.contains("Choose Retry"));
            }
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            assert!(client.input(b"\r"));
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { token: dismissed } if dismissed == token));
            assert!(client.state.lock().unwrap().view("host").is_none());
        }
    }

    #[test]
    fn long_filenames_scroll_without_hiding_destination_or_validation_error() {
        let root = std::env::temp_dir().join(format!("kraal-long-names-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let paths: Vec<_> = (0..32).map(|i| root.join(format!("{i}{}-end.txt", "x".repeat(190)))).collect();
        for path in &paths {
            std::fs::write(path, b"abc").unwrap();
        }
        for (cols, count) in [124, 80].into_iter().flat_map(|cols| [2, 32].map(|count| (cols, count))) {
            let (client, mut peer) = controller(true);
            let original = paths[..count].iter().map(|p| p.to_str().unwrap()).collect::<Vec<_>>().join(" ");
            let expected: Vec<_> = paths[..count]
                .iter()
                .flat_map(|p| {
                    crate::text::fold(p.file_name().unwrap().to_str().unwrap(), if cols == 124 { 96 } else { 72 })
                })
                .collect();
            prepare_drop(&client, &mut peer, original.as_bytes());
            {
                let mut state = client.state.lock().unwrap();
                let dropped = state.dropped.as_mut().unwrap();
                dropped.broadcast = true;
                dropped.limits.file_bytes = 1;
            }
            client.input(b"\r");
            assert!(client.state.lock().unwrap().active.is_none());
            let screen = Rect::new(0, 0, cols, 23);
            let mut seen = std::collections::HashSet::new();
            let mut previous = None;
            loop {
                let state = client.state.lock().unwrap();
                let view = state.view("this machine").unwrap();
                let layout = view.layout(screen);
                let (buffer, buttons) = view.render(screen);
                let rows: Vec<String> = (buffer.area.y..buffer.area.bottom())
                    .map(|y| (buffer.area.x..buffer.area.right()).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                    .collect();
                let shown = rows.join("\n");
                assert!(shown.contains("Their paths on this machine go to Claude."), "{shown}");
                assert!(shown.contains("Broadcasting is on. Paths go only to this pane."), "{shown}");
                assert!(shown.contains("file too large; use kraal cp or raise attachments.file_bytes"), "{shown}");
                assert_eq!(buttons.len(), 4);
                assert!(shown.contains("Send") && shown.contains("Cancel"));
                for line in &expected {
                    if rows.iter().any(|row| row.contains(line)) {
                        seen.insert(line.clone());
                    }
                }
                if count == 32 {
                    assert!(layout.max_scroll > 0);
                    assert!(shown.contains("PgUp/PgDn: filenames"));
                }
                if previous == Some(view.scroll) {
                    assert!(expected.iter().all(|line| seen.contains(line)), "some filename rows were never readable");
                    break;
                }
                previous = Some(view.scroll);
                drop(state);
                client.keys_with_screen(b"\x1b[6~", screen);
            }
            client.keys_with_screen(b"\x1b[5~", screen);
            if count == 32 {
                assert!(client.state.lock().unwrap().dropped.as_ref().unwrap().scroll < previous.unwrap());
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn image_offer_requires_local_enter_and_ordinary_paste_never_reads_the_clipboard() {
        for remote in [false, true] {
            let (client, mut peer) = controller(remote);
            client.connected();
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
    fn image_buttons_cancel_with_keyboard_and_mouse_without_reading() {
        for mouse in [false, true] {
            let (client, mut peer) = controller(true);
            client.message(ServerMsg::ClipboardImageOffer {
                token: "ab".into(),
                target: "main / main › dev@anvil:~".into(),
                limits: Default::default(),
            });
            let view = client.state.lock().unwrap().view("fakehost").unwrap();
            assert_eq!(view.title, "Send the clipboard image to fakehost?");
            assert_eq!(view.buttons, ["Send", "Cancel"]);
            assert!(view.message.starts_with("Its path on fakehost goes to dev@anvil:~."));
            assert!(peer.incoming.try_recv().is_err());
            if mouse {
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let (_, buttons) = view.render(Rect::new(0, 0, cols, rows - 1));
                let cancel = buttons[1];
                client.keys(format!("\x1b[<0;{};{}M", cancel.x + 1, cancel.y + 1).as_bytes());
            } else {
                client.keys(b"\t\r");
            }
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { .. }));
            assert!(client.state.lock().unwrap().active.is_none());
            assert!(client.state.lock().unwrap().workers.is_empty());
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
                client.connected();
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
            for attempt in 0..2 {
                let token = {
                    let state = client.state.lock().unwrap();
                    state
                        .prompt
                        .as_ref()
                        .map(|p| p.token.clone())
                        .or_else(|| state.dropped.as_ref().map(|d| d.token.clone()))
                        .unwrap()
                };
                client.keys(b"\r");
                let ClientMsg::AttachmentStart { files, .. } = read_without_repaint(&mut peer) else {
                    panic!("image did not use attachment transport")
                };
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].name, "clipboard.png");
                assert_eq!(files[0].size, 8);
                // Redraws share the writer with the upload worker, including between acknowledgements.
                send(&client.writer, &ClientMsg::Resize { cols: 90, rows: 30 }).unwrap();
                client.message(ServerMsg::AttachmentAck { token: token.clone(), received: 0 });
                let ClientMsg::AttachmentChunk { data, .. } = read_without_repaint(&mut peer) else { panic!() };
                assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), b"\x89PNG\r\n\x1a\n");
                send(&client.writer, &ClientMsg::Resize { cols: 91, rows: 30 }).unwrap();
                client.message(ServerMsg::AttachmentAck { token: token.clone(), received: 8 });
                assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentFinish { .. }));
                if attempt == 0 {
                    client.message(ServerMsg::AttachmentResult {
                        kind: NoticeKind::Error,
                        token,
                        message: "Transfer interrupted. Choose Retry to try again.".into(),
                        pasted: false,
                    });
                    assert_eq!(client.state.lock().unwrap().view("server").unwrap().buttons, ["Retry", "Close"]);
                    let reads = std::fs::read_to_string(&log).unwrap();
                    client.keys(b"\t\r");
                    assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                    let ClientMsg::AttachmentRequest { token, image: true, .. } = read_without_repaint(&mut peer)
                    else {
                        panic!()
                    };
                    client.message(ServerMsg::AttachmentPrepared {
                        token,
                        target: "pane".into(),
                        limits: Default::default(),
                        broadcast: false,
                    });
                    assert!(client.state.lock().unwrap().active.is_none());
                    assert_eq!(std::fs::read_to_string(&log).unwrap(), reads, "retry must wait for local Send");
                    assert_eq!(client.state.lock().unwrap().view("server").unwrap().buttons, ["Send", "Cancel"]);
                }
            }
            client.shutdown();
        }
        assert_eq!(std::fs::read_to_string(log).unwrap(), "read\nread\nread\nread\n");
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
                if let Some(pid) = crate::testing::read_complete_lines(&ready).and_then(|text| text.trim().parse().ok())
                {
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
                    last_draw: None,
                    label: String::new(),
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
            host: "alice@box".into(),
            session: "main".into(),
            label: "box".into(),
            pane: None,
            clipboard_native: false,
        };
        let (writer, mut peer) = wire();
        let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target, None);
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
            let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target, None);
            client.connected();
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
    fn paste_recognition_requires_remote_connection_and_preserves_ordinary_bytes() {
        let path = dropped_file("ordinary");
        for remote in [false, true] {
            let (client, peer) = controller(remote);
            let input = paste::bracket(path.to_str().unwrap().as_bytes());
            assert!(!client.input(&input));
            assert!(peer.incoming.try_recv().is_err());
            if !remote {
                client.connected();
                assert!(!client.input(&input));
            }
        }
        let (client, mut peer) = controller(true);
        client.connected();
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
        client.connected();
        client.input(&[paste::bracket(&original), b"\r".to_vec()].concat());
        assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
        let ClientMsg::AttachmentRequest { token, text, image: false, .. } = read(&mut peer) else { panic!() };
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
                        ClientMsg::Repaint => {}
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
                    client.connected();
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
                    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
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
                client.connected();
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
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        for bytes in [b"later".as_slice(), b" typing", b"\r", &paste::bracket(b"instructions")] {
            client.input(bytes);
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, bytes.to_vec()));
        }
        assert!(client.state.lock().unwrap().uncertain.is_some());
        client.message(ServerMsg::AttachmentResult {
            kind: NoticeKind::Notice,
            token,
            message: "Text submitted.".into(),
            pasted: true,
        });
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
                            kind: NoticeKind::Error,
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
                    last_draw: None,
                    label: String::new(),
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
                assert!(status.contains("Image not sent:"));
                assert!(status.contains("Choose Retry"));
            } else {
                assert!(status.contains("Choose Retry"));
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
                kind: NoticeKind::Error,
                token: token.clone(),
                message: "Transfer interrupted. No paths pasted.".into(),
                pasted: false,
            });
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            client.expire_success(Instant::now() + Duration::from_secs(60));
            assert_eq!(
                client.state.lock().unwrap().view("server").unwrap().buttons,
                ["Retry", "Paste as text", "Close"]
            );
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
        client.message(ServerMsg::AttachmentResult {
            kind: NoticeKind::Notice,
            token,
            message: "Files received.".into(),
            pasted: true,
        });
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
    fn terminal_documentation_shows_the_rendered_questions() {
        let root = std::env::temp_dir().join(format!("kraal-attachment-mockups-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("capture one.png");
        let second = root.join("notes.txt");
        std::fs::write(&first, vec![0; 300 * 1024]).unwrap();
        std::fs::write(&second, b"notes").unwrap();
        let (client, mut peer) = controller(true);
        prepare_drop(&client, &mut peer, format!("'{}' '{}'", first.display(), second.display()).as_bytes());
        let mut state = client.state.lock().unwrap();
        state.dropped.as_mut().unwrap().target = "dev@anvil:~".into();
        let view = state.view("SERVER").unwrap();
        for cols in [124, 80] {
            let (buf, _) = view.render(Rect::new(0, 0, cols, 23));
            let text = buf
                .content
                .chunks(buf.area.width as usize)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(include_str!("../../docs/terminal.md").contains(&text), "missing {cols}-column mockup:\n{text}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn question_boxes_restore_every_cell_at_124_and_80_columns() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;
        struct Size(usize, usize);
        impl Dimensions for Size {
            fn total_lines(&self) -> usize {
                self.1
            }
            fn screen_lines(&self) -> usize {
                self.1
            }
            fn columns(&self) -> usize {
                self.0
            }
        }
        let path = dropped_file("layout");
        for (cols, phase) in [124, 80]
            .into_iter()
            .flat_map(|cols| ["choice", "image", "progress", "failure", "success"].map(|phase| (cols, phase)))
        {
            let (client, mut peer) = controller(true);
            let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
            {
                let mut state = client.state.lock().unwrap();
                match phase {
                    "image" => state.dropped.as_mut().unwrap().image = true,
                    "progress" => {
                        state.active = Some(Active {
                            token: token.clone(),
                            cancelled: Arc::new(AtomicBool::new(false)),
                            ack: bounded(2).0,
                            started: Instant::now(),
                            last_draw: None,
                            label: String::new(),
                            progress: None,
                            failure: None,
                        });
                        state.status = Some("0.1 / 0.3 MiB (33%)".into());
                    }
                    "failure" => {
                        state.dropped.as_mut().unwrap().result = true;
                        state.status = Some("Transfer interrupted. Choose Retry to try again.".into());
                        state.result_token = Some(token.clone());
                    }
                    "success" => {
                        state.dropped = None;
                        state.status = Some("Sent 1 file to fakehost".into());
                        state.success_until = Some(Instant::now());
                    }
                    _ => {}
                }
            }
            let screen = Rect::new(0, 0, cols, 24);
            let mut background = Buffer::empty(screen);
            for pos in screen.positions() {
                background[pos]
                    .set_symbol(if pos.y == 23 {
                        "━"
                    } else if pos.x == 20 {
                        "┃"
                    } else {
                        "x"
                    })
                    .set_fg(ratatui::style::Color::Blue)
                    .set_bg(ratatui::style::Color::DarkGray);
            }
            let mut encoder = crate::ansi::Encoder::new();
            let mut bytes = Vec::new();
            let cursor =
                Some(crate::ansi::Cursor { x: 25, y: 22, shape: crate::ansi::CursorShape::Bar, blinking: false });
            encoder.encode_with_images(&background, &[], cursor, &Default::default(), &[], &[], &mut bytes);
            let mut terminal = Term::new(Config::default(), &Size(cols as usize, 24), VoidListener);
            let mut parser: Processor = Processor::new();
            parser.advance(&mut terminal, &bytes);
            let before = terminal.grid().clone();
            let view = client.state.lock().unwrap().view("fakehost").unwrap();
            let (buf, buttons) = view.render(Rect::new(0, 0, cols, 23));
            if phase == "choice" {
                assert_eq!(view.buttons, ["Send", "Always for fakehost", "Paste as text", "Cancel"]);
                assert_eq!(view.title, "Send 1 file to fakehost?");
                assert_eq!(buttons.len(), 4);
            }
            for (index, rect) in buttons.iter().enumerate() {
                assert!(buf.area.contains((rect.right() - 1, rect.y).into()));
                let style = overlays::button_style(index == 0, false);
                let cell = &buf[(rect.x, rect.y)];
                assert_eq!(Some(cell.fg), style.fg);
                assert_eq!(cell.bg, style.bg.unwrap_or_default());
                assert_eq!(cell.modifier, style.add_modifier);
            }
            let mode = *terminal.mode();
            parser.advance(&mut terminal, &crate::ansi::overlay(&buf));
            assert_eq!(terminal.grid().cursor.point, before.cursor.point);
            assert_eq!(*terminal.mode(), mode);
            for pos in screen.positions().filter(|pos| !buf.area.contains(*pos)) {
                assert_eq!(
                    terminal.grid()[Line(pos.y as i32)][Column(pos.x as usize)],
                    before[Line(pos.y as i32)][Column(pos.x as usize)]
                );
            }
            if phase == "success" {
                client.expire_success(Instant::now());
            } else {
                client.keys(if phase == "progress" { b"\x03" } else { b"\x1b" });
                if phase == "failure" {
                    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                }
                client.keys(&[]);
                let message = read(&mut peer);
                if phase == "failure" {
                    assert!(matches!(message, ClientMsg::AttachmentDismiss { token: sent } if sent == token));
                } else {
                    assert!(matches!(message, ClientMsg::AttachmentCancel { token: sent } if sent == token));
                }
                if phase == "progress" {
                    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                    client.message(ServerMsg::AttachmentResult {
                        kind: NoticeKind::Error,
                        token: token.clone(),
                        message: "Transfer cancelled.".into(),
                        pasted: false,
                    });
                    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                    client.keys(b"\x1b");
                    client.keys(&[]);
                    assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                }
            }
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            assert!(client.state.lock().unwrap().view("fakehost").is_none());
            encoder.invalidate();
            bytes.clear();
            encoder.encode_with_images(&background, &[], cursor, &Default::default(), &[], &[], &mut bytes);
            parser.advance(&mut terminal, &bytes);
            for pos in screen.positions() {
                assert_eq!(
                    terminal.grid()[Line(pos.y as i32)][Column(pos.x as usize)],
                    before[Line(pos.y as i32)][Column(pos.x as usize)]
                );
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn compact_boxes_and_every_button_at_124_and_80_columns() {
        if !crate::testing::in_own_process(
            "client::attachments::tests::compact_boxes_and_every_button_at_124_and_80_columns",
        ) {
            return;
        }
        let root = crate::testing::TempDir::new("attachment-buttons");
        let path = root.0.join("capture.png");
        std::fs::write(&path, b"image").unwrap();
        #[cfg(not(target_os = "macos"))]
        let (tool, script) = ("wl-paste", "#!/bin/sh\nprintf '\\211PNG\\r\\n\\032\\n'\n");
        #[cfg(target_os = "macos")]
        let (tool, script) =
            ("osascript", "#!/bin/sh\nfor arg; do file=$arg; done\nprintf '\\211PNG\\r\\n\\032\\n' > \"$file\"\n");
        crate::testing::write_script(&root.0.join(tool), script);
        // This isolated test has not started workers yet.
        unsafe {
            std::env::set_var("PATH", &root.0);
            std::env::set_var("WAYLAND_DISPLAY", "test");
            std::env::remove_var("DISPLAY");
        }
        for cols in [124, 80] {
            for (phase, height, labels, expected) in [
                ("question", 7, vec!["Send", "Always for this machine", "Paste as text", "Cancel"], "capture.png"),
                ("image", 6, vec!["Send", "Cancel"], "Its path on this machine goes to Claude."),
                (
                    "shortcut",
                    6,
                    vec!["Send", "Always for this machine", "Cancel"],
                    "Its path on this machine goes to Claude.",
                ),
                ("prompt", 8, vec!["Send", "Cancel"], "Enter absolute local paths, quoting spaces."),
                ("image prompt", 7, vec!["Send", "Cancel"], "Send reads the local clipboard once."),
                ("progress", 9, vec!["Cancel"], "0 B / 4.0 MiB (0%)"),
                (
                    "failure",
                    6,
                    vec!["Retry", "Paste as text", "Close"],
                    "Transfer interrupted. Choose Retry to try again.",
                ),
                ("image failure", 6, vec!["Retry", "Close"], "Transfer interrupted. Choose Retry to try again."),
                ("result", 6, vec!["Close"], "No paths pasted. Close and try again."),
            ] {
                for index in 0..labels.len() {
                    let (client, mut peer) = controller(true);
                    let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
                    {
                        let mut state = client.state.lock().unwrap();
                        if phase.contains("image") || phase == "shortcut" {
                            let dropped = state.dropped.as_mut().unwrap();
                            dropped.image = true;
                            dropped.shortcut = phase == "shortcut";
                            dropped.paths.clear();
                        }
                        if phase.contains("prompt") {
                            state.dropped = None;
                            state.prompt = Some(Prompt {
                                image: phase == "image prompt",
                                token: token.clone(),
                                target: "Claude".into(),
                                limits: Default::default(),
                                text: path.to_str().unwrap().into(),
                                cursor: 0,
                                error: None,
                                paste_failed: false,
                                parser: InputParser::new(),
                            });
                        } else if phase == "progress" {
                            state.active = Some(Active {
                                token: token.clone(),
                                cancelled: Arc::new(AtomicBool::new(false)),
                                ack: bounded(2).0,
                                started: Instant::now(),
                                last_draw: None,
                                label: "capture.png".into(),
                                progress: Some((0, 4 * 1048576)),
                                failure: None,
                            });
                            state.displayed =
                                state.active.as_ref().unwrap().transfer(crate::theme::current(), Instant::now(), false);
                            state.status = Some("Typing waits until it finishes.".into());
                        } else if phase.contains("failure") {
                            state.dropped.as_mut().unwrap().result = true;
                            state.status = Some(expected.into());
                            state.result_token = Some(token.clone());
                        } else if phase == "result" {
                            state.dropped = None;
                            state.status = Some(expected.into());
                            state.result_token = Some(token.clone());
                        }
                    }
                    let screen = Rect::new(0, 0, cols, 35);
                    let view = client.state.lock().unwrap().view("this machine").unwrap();
                    assert_eq!(view.buttons, labels);
                    let (buf, buttons) = view.render(screen);
                    assert_eq!(buf.area.width, if cols == 124 { 100 } else { 76 });
                    assert_eq!(buf.area.height, height, "{phase} at {cols}");
                    let rows = buffer_rows(&buf);
                    assert!(rows.join("\n").contains(expected), "{phase}: {rows:?}");
                    assert_eq!(buttons.len(), labels.len());
                    assert_eq!(buttons[index].y, buf.area.bottom() - 3);
                    assert!(rows[height as usize - 4].trim_matches(['│', ' ']).is_empty());
                    assert!(rows[height as usize - 2].contains("Enter") || phase == "progress");
                    let rect = buttons[index];
                    assert_eq!(client.button_at(&client.state.lock().unwrap(), screen, rect.x, rect.y - 1), None);
                    client.keys_with_screen(format!("\x1b[<0;{};{}M", rect.x + 1, rect.y + 1).as_bytes(), screen);
                    match labels[index] {
                        "Send" | "Always for this machine" => {
                            assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentStart { .. }));
                            assert!(client.state.lock().unwrap().active.is_some());
                            client.shutdown();
                        }
                        "Retry" => {
                            assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                            assert!(matches!(read(&mut peer), ClientMsg::AttachmentRequest { .. }));
                        }
                        "Paste as text" if phase == "question" => {
                            assert!(matches!(read(&mut peer), ClientMsg::AttachmentText { .. }));
                        }
                        "Paste as text" => {
                            assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                            assert_eq!(
                                protocol::read_frame(&mut peer).unwrap().unwrap(),
                                (protocol::BYTES, paste::bracket(path.to_str().unwrap().as_bytes()))
                            );
                        }
                        "Close" => assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. })),
                        "Cancel" => assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { .. })),
                        _ => unreachable!(),
                    }
                }
            }
        }
    }

    fn buffer_rows(buf: &Buffer) -> Vec<String> {
        buf.content.chunks(buf.area.width as usize).map(|row| row.iter().map(|cell| cell.symbol()).collect()).collect()
    }

    #[test]
    fn final_transfer_keeps_its_content_and_closes_automatically() {
        let path = dropped_file("final");
        for cols in [124, 80] {
            for (image, count, label) in [(false, 2, "2 files"), (false, 1, "capture.png"), (true, 1, "clipboard.png")]
            {
                for focus_changed in [false, true] {
                    let (client, mut peer) = controller(true);
                    let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
                    {
                        let mut state = client.state.lock().unwrap();
                        let dropped = state.dropped.as_mut().unwrap();
                        dropped.image = image;
                        dropped.paths = vec![path.clone(); count];
                        state.active = Some(Active {
                            token: token.clone(),
                            cancelled: Arc::new(AtomicBool::new(false)),
                            ack: bounded(2).0,
                            started: Instant::now(),
                            last_draw: None,
                            label: label.into(),
                            progress: Some((57 * 1048576, 57 * 1048576)),
                            failure: None,
                        });
                    }
                    let before = Instant::now();
                    client.message(ServerMsg::AttachmentResult {
                        token,
                        pasted: true,
                        kind: if focus_changed { NoticeKind::Warning } else { NoticeKind::Notice },
                        message: if focus_changed { "Files received. Focus changed." } else { "Files received." }
                            .into(),
                    });
                    assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                    let (mut view, until) = {
                        let state = client.state.lock().unwrap();
                        (state.view("SERVER").unwrap(), state.success_until.unwrap())
                    };
                    // Both themes use the same final size and geometry.
                    for ascii in [false, true] {
                        let transfer = view.transfer.as_mut().unwrap();
                        transfer.ascii = ascii;
                        transfer.marker = if ascii { 'v' } else { '✓' };
                        let (buf, buttons) = view.render(Rect::new(0, 0, cols, 35));
                        assert_eq!(buf.area.height, 6);
                        assert!(buttons.is_empty());
                        let rows = buffer_rows(&buf);
                        assert!(rows[0].contains("Sent to this machine"));
                        assert!(rows[1].contains(label));
                        assert!(rows[2].contains("57 MiB / 57 MiB (100%)"));
                        assert!(rows[3].contains(&format!(
                            "{} {}",
                            if ascii { 'v' } else { '✓' },
                            if ascii { "#" } else { "▓" }.repeat(buf.area.width as usize - 6)
                        )));
                        assert!(rows[4].contains(if focus_changed {
                            "Focus changed. Select Claude before typing again."
                        } else if image || count == 1 {
                            "Path pasted into Claude."
                        } else {
                            "Paths pasted into Claude."
                        }));
                        assert!(!rows.join("\n").contains("Typing waits"));
                    }
                    assert!(until >= before + SUCCESS_DURATION);
                    assert!(until <= Instant::now() + SUCCESS_DURATION);
                    client.input(b"typing\x03");
                    assert_eq!(
                        protocol::read_frame(&mut peer).unwrap().unwrap(),
                        (protocol::BYTES, b"typing\x03".to_vec())
                    );
                    client.expire_success(until);
                }
            }
        }
        let (client, mut peer, _) = progress_client();
        client.message(ServerMsg::AttachmentResult {
            token: "ab".into(),
            pasted: true,
            kind: NoticeKind::Notice,
            message: "Files received.".into(),
        });
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        assert!(client.state.lock().unwrap().view("SERVER").is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn success_expires_without_swallowing_typing_or_control_c() {
        let path = dropped_file("notice");
        for image in [false, true] {
            let (client, mut peer) = controller(true);
            let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
            client.state.lock().unwrap().dropped.as_mut().unwrap().image = image;
            client.message(ServerMsg::AttachmentResult {
                kind: NoticeKind::Notice,
                token: token.clone(),
                message: "Files received. Paths submitted to the reserved pane. No Enter added.".into(),
                pasted: true,
            });
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { token: sent } if sent == token));
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            let until = {
                let state = client.state.lock().unwrap();
                assert_eq!(state.status.as_deref(), Some("Sent to this machine\nPath pasted into Claude."));
                assert!(state.view("this machine").unwrap().buttons.is_empty());
                state.success_until.unwrap()
            };
            for keys in [b"typing".as_slice(), b"\x03", b"\x1b[99;5u"] {
                client.input(keys);
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, keys.to_vec()));
            }
            client.expire_success(until - Duration::from_millis(1));
            assert!(client.state.lock().unwrap().status.is_some());
            client.expire_success(until);
            assert!(client.state.lock().unwrap().view("this machine").is_none());
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            client.input(b"\x03");
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"\x03".to_vec()));
        }
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
                                last_draw: None,
                                label: String::new(),
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
            assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentStart { .. }));
            if acknowledge_start {
                client.message(ServerMsg::AttachmentAck { token: "ab".into(), received: 0 });
                assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentChunk { .. }));
            }
            let started = Instant::now();
            loop {
                if let Some(view) = client.state.lock().unwrap().view("SERVER") {
                    let (buf, _) = view.render(Rect::new(0, 0, 80, 23));
                    let status = buffer_rows(&buf).join("\n");
                    assert!(status.contains(path.file_name().unwrap().to_str().unwrap()));
                    assert!(status.contains("0 B / 3 B (0%)"));
                    assert!(status.contains("Typing waits"));
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
            last_draw: None,
            label: String::new(),
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
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        client.keys(b"\x1b[200~");
        client.keys(&vec![b'x'; 128 * 1024]);
        client.keys(format!(" {}\x1b[201~\r", path.display()).as_bytes());
        assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().error.is_some());
        assert!(client.state.lock().unwrap().active.is_none());
        client.keys(b"\x15");
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
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
                last_draw: None,
                label: String::new(),
                progress: None,
                failure: None,
            });
        }
        client.message(ServerMsg::AttachmentResult {
            kind: NoticeKind::Notice,
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
