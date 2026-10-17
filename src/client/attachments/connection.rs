use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};

use super::view::{friendly_result, pane_name};
use super::{Controller, Prompt, SUCCESS_DURATION, Writer};
use crate::client::paste;
use crate::input::InputParser;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::theme::NoticeKind;
use crate::{attachments, protocol};

pub(super) fn send(writer: &Writer, msg: &ClientMsg) -> Result<()> {
    let mut writer = writer.lock().unwrap();
    protocol::write_json(writer.as_mut().context("connection closed")?, msg)?;
    Ok(())
}

impl Controller {
    pub fn connected(&self) {
        self.state.lock().unwrap().connected = true;
    }

    pub fn message(self: &Arc<Self>, msg: ServerMsg) {
        let mut state = self.state.lock().unwrap();
        let repaint = matches!(&msg, ServerMsg::AttachmentResult { .. });
        let mut success = false;
        let image = matches!(&msg, ServerMsg::ClipboardImageOffer { .. });
        match msg {
            ServerMsg::ClipboardFocus { pane, agent } => {
                if state.focus != Some((pane, agent))
                    && let Some(read) = state.clipboard_read.take()
                {
                    read.cancelled.store(true, Ordering::SeqCst);
                }
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
                let automatic = (!dropped.retry || !self.remote && image && shortcut)
                    && dropped.selected == 0
                    && dropped.error.is_none()
                    && (!broadcast || !self.remote && image)
                    && if image {
                        shortcut
                            && (!self.remote
                                || state.image_approved
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
                    // An image question selects Cancel: the server can open it while Enter is typed for a pane.
                    cursor: usize::from(image),
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
        if let Some(read) = state.clipboard_read.take() {
            read.cancelled.store(true, Ordering::SeqCst);
        }
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
