use std::sync::Arc;

use anyhow::Result;
use crossbeam_channel::bounded;
use ratatui::layout::Rect;

use super::connection::send;
use super::transfer::explicit_paths;
use super::{Controller, Dropped, Prompt};
use crate::client::clipboard_image::Diagnostic;
use crate::client::paste;
use crate::input::{InputEvent, InputParser, KeyCode};
use crate::protocol::{self, ClientMsg};

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
}

impl Controller {
    pub fn pending_keys(&self) -> bool {
        let state = self.state.lock().unwrap();
        self.laptop.as_ref().is_some_and(|laptop| laptop.pending_keys())
            || state.image_keys.pending()
            || state.splitter.has_pending()
            || state.dropped.is_some() && state.parser.has_pending()
            || state.prompt.as_ref().is_some_and(|p| p.parser.has_pending())
    }

    pub fn input(self: &Arc<Self>, bytes: &[u8]) -> bool {
        self.input_with_probe(bytes, super::super::clipboard_image::available)
    }

    pub(super) fn input_with_probe(
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

    pub(super) fn forward(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        if let Some(writer) = writer.as_mut() {
            let _ = protocol::write_frame(writer, protocol::BYTES, bytes);
        }
    }

    /// True when the local form consumed the input. Normal typing stays on the same wire.
    pub fn keys(self: &Arc<Self>, bytes: &[u8]) -> bool {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        self.keys_with_screen(bytes, Rect::new(0, 0, cols, rows.saturating_sub(1)))
    }

    pub(super) fn keys_with_screen(self: &Arc<Self>, bytes: &[u8], screen: Rect) -> bool {
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

    pub(super) fn drop_action(self: &Arc<Self>, action: usize) {
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
}
