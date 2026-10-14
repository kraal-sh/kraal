//! Attachment state belongs to a connection; completed files belong to the session.

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use base64::Engine;

use super::{ClientId, Event, Mode, Server};
use crate::attachments::{self, Batch};
use crate::layout::PaneId;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::theme::NoticeKind;

#[derive(Default)]
pub(super) struct Attachments {
    pub negotiated: bool,
    pub recognition: bool,
    pub images: bool,
    pub agent_clipboard: bool,
    pub focus: Option<(Option<PaneId>, bool)>,
    pub pending: Option<Pending>,
    pub blocked: Option<String>,
}

pub(super) struct Pending {
    token: String,
    from_paste: bool,
    image: bool,
    pane: PaneId,
    started: Instant,
    batch: bool,
    busy: bool,
    cancelled: Arc<AtomicBool>,
    keys: Vec<u8>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

pub enum Completion {
    Ack(Result<u64>),
    Finished(Result<String>),
    Status(String, NoticeKind),
    Purged(std::io::Result<bool>),
    Recorded(Result<()>),
}

type Batches = HashMap<String, (Batch, Arc<AtomicBool>)>;
type Work = Box<dyn FnOnce(&mut Batches) -> Completion + Send>;

struct Job {
    id: ClientId,
    token: String,
    work: Work,
}

#[cfg(test)]
type Pause = Arc<std::sync::Mutex<Option<(crossbeam_channel::Sender<()>, crossbeam_channel::Receiver<()>)>>>;

pub(super) struct Worker {
    jobs: crossbeam_channel::Sender<Job>,
    #[cfg(test)]
    pause: Pause,
}

impl Worker {
    fn start(events: super::EventSender) -> Result<Self> {
        let (jobs, incoming) = crossbeam_channel::bounded::<Job>(32);
        #[cfg(test)]
        let pause =
            Arc::new(std::sync::Mutex::new(None::<(crossbeam_channel::Sender<()>, crossbeam_channel::Receiver<()>)>));
        #[cfg(test)]
        let paused = pause.clone();
        crate::threads::named("attachment-storage").spawn(move || {
            let mut batches = Batches::new();
            loop {
                let job = incoming.recv_timeout(Duration::from_millis(100));
                // Cleanup, including Batch::drop, stays on this thread even after disconnect.
                batches.retain(|_, (_, cancelled)| !cancelled.load(Ordering::SeqCst));
                match job {
                    Ok(job) => {
                        #[cfg(test)]
                        if let Some((entered, resume)) = paused.lock().unwrap().take() {
                            entered.send(()).unwrap();
                            resume.recv().unwrap();
                        }
                        let result = (job.work)(&mut batches);
                        if events.send(Event::Attachment(job.id, job.token, result)).is_err() {
                            break;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
            }
        })?;
        Ok(Self {
            jobs,
            #[cfg(test)]
            pause,
        })
    }
}

impl Server {
    pub(super) fn clipboard_focus(&self, id: ClientId) -> Option<(Option<PaneId>, bool)> {
        let client = self.clients.get(&id)?;
        if !client.attachments.agent_clipboard {
            return None;
        }
        let pane = self.client_focus(id);
        let agent = matches!(client.mode, Mode::Normal)
            && pane
                .and_then(|pane| self.panes.get(&pane))
                .is_some_and(|pane| pane.agent.kind.is_some() && pane.exit_code.is_none() && !pane.shell_waiting());
        Some((pane, agent))
    }

    pub(super) fn offer_attachments(&mut self, id: ClientId, confirmed: Option<PaneId>) -> Mode {
        self.offer_attachment(id, confirmed, false)
    }

    pub(super) fn offer_attachment(&mut self, id: ClientId, confirmed: Option<PaneId>, image: bool) -> Mode {
        let Some(c) = self.clients.get(&id) else { return Mode::Normal };
        let action = if image { "Attach clipboard image" } else { "Attach files" };
        if !c.attached || !c.attachments.negotiated {
            self.toast(
                id,
                NoticeKind::Warning,
                format!("{action} needs a newer client. Update kraal on your laptop and attach again."),
            );
            return Mode::Normal;
        }
        if image && !c.attachments.images {
            self.toast(
                id,
                NoticeKind::Warning,
                "Attach clipboard image needs a newer client. Update kraal on your laptop and attach again.",
            );
            return Mode::Normal;
        }
        if c.attachments.blocked.is_some() {
            self.toast(id, NoticeKind::Warning, "Close the attachment result before trying again.");
            return Mode::Normal;
        }
        if c.attachments.pending.is_some() {
            self.toast(id, NoticeKind::Warning, "An attachment is already open. Finish it or press Ctrl+C to cancel.");
            return Mode::Normal;
        }
        let Some(pane) = confirmed.or_else(|| self.client_focus(id)) else {
            self.toast(id, NoticeKind::Warning, "Add a terminal pane before attaching files.");
            return Mode::Normal;
        };
        if !self.attachment_target(pane) {
            self.toast(id, NoticeKind::Warning, "Select a running terminal pane before attaching files.");
            return Mode::Normal;
        }
        if confirmed.is_none() && self.input_targets(id, pane).len() != 1 {
            return Mode::confirm(
                format!("Broadcasting is on. {action} only to {}?", self.pane_label(pane)),
                if image {
                    super::ui::ConfirmAction::AttachClipboardImage(pane)
                } else {
                    super::ui::ConfirmAction::AttachFiles(pane)
                },
            );
        }
        let token = format!(
            "{:x}-{:x}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(),
            self.alloc_id()
        );
        let target = self.panes[&pane].display_name();
        self.clients.get_mut(&id).unwrap().attachments.pending = Some(Pending {
            token: token.clone(),
            from_paste: false,
            image,
            pane,
            started: Instant::now(),
            batch: false,
            busy: false,
            cancelled: Arc::new(AtomicBool::new(false)),
            keys: Vec::new(),
        });
        let mut limits = self.config.attachments.clone();
        if image {
            limits.file_bytes = limits.file_bytes.min(attachments::IMAGE_BYTES as u64);
            limits.files = 1;
        }
        let offer = if image {
            ServerMsg::ClipboardImageOffer { token, target, limits }
        } else {
            ServerMsg::AttachmentOffer { token, target, limits }
        };
        self.send_msg(id, &offer);
        Mode::Normal
    }

    fn request_attachment(&mut self, id: ClientId, token: String, text: Vec<u8>, image: bool) {
        if !attachments::token_valid(&token) || text.len() > 64 * 1024 {
            return;
        }
        if !image
            && !matches!(self.clients[&id].mode, Mode::Normal)
            && self.clients[&id].attachments.pending.is_none()
            && self.clients[&id].attachments.blocked.is_none()
        {
            self.client_input(id, &[b"\x1b[200~".as_slice(), &text, b"\x1b[201~"].concat());
            self.send_msg(
                id,
                &ServerMsg::AttachmentResult { kind: NoticeKind::Notice, token, message: String::new(), pasted: true },
            );
            return;
        }
        let reserved = self.clients.get_mut(&id).unwrap().paste.take();
        let (pane, broadcast) = match reserved {
            Some(super::input::PasteFlow::Held { focused, targets, .. }) => (Some(focused), targets.len() != 1),
            Some(_) => (None, false),
            None => {
                let pane = self.client_focus(id);
                (pane, pane.is_some_and(|pane| self.input_targets(id, pane).len() != 1))
            }
        };
        let error =
            if self.clients[&id].attachments.pending.is_some() || self.clients[&id].attachments.blocked.is_some() {
                Some("Finish or dismiss the current attachment first.")
            } else if pane.is_none_or(|pane| !self.attachment_target(pane)) {
                Some("Select a running terminal pane, then retry.")
            } else {
                None
            };
        if let Some(error) = error {
            self.send_msg(
                id,
                &ServerMsg::AttachmentResult {
                    kind: NoticeKind::Error,
                    pasted: false,
                    token,
                    message: format!("No paths pasted. {error}"),
                },
            );
            return;
        }
        let pane = pane.unwrap();
        let target = self.panes[&pane].display_name();
        self.clients.get_mut(&id).unwrap().attachments.pending = Some(Pending {
            token: token.clone(),
            from_paste: !image,
            image,
            pane,
            started: Instant::now(),
            batch: false,
            busy: false,
            cancelled: Arc::new(AtomicBool::new(false)),
            keys: Vec::new(),
        });
        self.send_msg(
            id,
            &ServerMsg::AttachmentPrepared { token, target, limits: self.config.attachments.clone(), broadcast },
        );
    }

    fn attachment_target(&self, pane: PaneId) -> bool {
        self.panes.get(&pane).is_some_and(|p| p.is_terminal() && p.exit_code.is_none())
    }

    pub(super) fn attachment_input(&mut self, id: ClientId, bytes: &[u8]) -> bool {
        let Some(client) = self.clients.get_mut(&id) else { return false };
        if client.attachments.blocked.is_some() {
            return true;
        }
        let Some(pending) = client.attachments.pending.as_mut() else { return false };
        if pending.keys.len() + bytes.len() > 64 * 1024 {
            let token = pending.token.clone();
            self.attachment_failed(id, token, "too much typing while sending; attach again after typing");
        } else {
            pending.keys.extend_from_slice(bytes);
        }
        true
    }

    fn attachment_failed(&mut self, id: ClientId, token: String, error: &str) {
        let image = self
            .clients
            .get(&id)
            .and_then(|c| c.attachments.pending.as_ref())
            .is_some_and(|p| p.token == token && p.image);
        let from_paste = self
            .clients
            .get(&id)
            .and_then(|c| c.attachments.pending.as_ref())
            .is_some_and(|p| p.token == token && p.from_paste);
        if let Some(c) = self.clients.get_mut(&id)
            && c.attachments.pending.as_ref().is_none_or(|p| p.token == token)
        {
            c.attachments.pending = None;
            c.attachments.blocked = Some(token.clone());
        }
        let message = if from_paste {
            format!("Transfer interrupted: {error}. No paths or queued typing pasted.")
        } else {
            format!(
                "Attachment stopped: {error}. No paths or queued typing pasted. Close this result, then use {} to retry.",
                if image { "Attach clipboard image" } else { "Attach files" }
            )
        };
        self.toast(id, NoticeKind::Error, &message);
        self.send_msg(id, &ServerMsg::AttachmentResult { kind: NoticeKind::Error, pasted: false, token, message });
    }

    pub(super) fn attachment_message(&mut self, id: ClientId, msg: ClientMsg) {
        if !self.clients.get(&id).is_some_and(|c| c.attached && c.attachments.negotiated) {
            return;
        }
        if matches!(msg, ClientMsg::AttachmentRequest { image: false, .. } | ClientMsg::AttachmentText { .. })
            && !self.clients[&id].attachments.recognition
        {
            return;
        }
        if let ClientMsg::AttachmentRequest { token, text, image, pane } = msg {
            if image && !self.clients[&id].attachments.images {
                return;
            }
            if !attachments::token_valid(&token) || text.len() > 64 * 1024 {
                return;
            }
            if let Some(pane) = pane
                && (!image
                    || !self.clients[&id].attachments.agent_clipboard
                    || self.clipboard_focus(id) != Some((Some(pane), true)))
            {
                // The request occupies the original key's place on the input wire. Replay it
                // here, before later typing, when the client's focus information was stale.
                self.client_input(id, &text);
                self.send_msg(
                    id,
                    &ServerMsg::AttachmentResult {
                        kind: NoticeKind::Notice,
                        token,
                        pasted: true,
                        message: String::new(),
                    },
                );
                return;
            }
            self.request_attachment(id, token, text, image);
            return;
        }
        let token = match &msg {
            ClientMsg::AttachmentText { token, .. }
            | ClientMsg::AttachmentStart { token, .. }
            | ClientMsg::AttachmentChunk { token, .. }
            | ClientMsg::AttachmentFinish { token }
            | ClientMsg::AttachmentCancel { token }
            | ClientMsg::AttachmentStatus { token }
            | ClientMsg::AttachmentDismiss { token }
            | ClientMsg::AttachmentLocal { token, .. } => token.clone(),
            _ => return,
        };
        if matches!(msg, ClientMsg::AttachmentDismiss { .. }) {
            let state = &mut self.clients.get_mut(&id).unwrap().attachments;
            if state.blocked.as_ref() == Some(&token) {
                state.blocked = None;
            }
            return;
        }
        let status = matches!(msg, ClientMsg::AttachmentStatus { .. });
        if status || self.clients[&id].attachments.pending.as_ref().is_none_or(|p| p.token != token) {
            // A repeated finish reports the durable result, never pastes again.
            let session = self.session.clone();
            let lookup = token.clone();
            if let Err(error) = self.attachment_job(id, token.clone(), move |_| {
                let (message, kind) = attachments::result(&session, &lookup).unwrap_or_else(|_| {
                    let message = if status {
                        "No completed batch found. No automatic retry. Inspect the pane, then use Attach files to retry.".into()
                    } else {
                        "Attachment expired. Use Attach files to retry.".into()
                    };
                    (message, NoticeKind::Warning)
                });
                Completion::Status(message, kind)
            }) {
                self.send_msg(id, &ServerMsg::AttachmentResult { kind: NoticeKind::Error, pasted: false, token, message: format!("Cannot read attachment result: {error}") });
            }
            return;
        }
        if matches!(msg, ClientMsg::AttachmentCancel { .. }) {
            self.attachment_failed(id, token, "cancelled");
            return;
        }
        if let Err(error) = self.receive_attachment(id, msg) {
            self.attachment_failed(id, token, &format!("{error:#}"));
        }
    }

    fn receive_attachment(&mut self, id: ClientId, msg: ClientMsg) -> Result<()> {
        let pending = self.clients[&id].attachments.pending.as_ref().unwrap();
        ensure!(self.attachment_target(pending.pane), "the reserved pane closed");
        ensure!(pending.started.elapsed() < attachments::TIMEOUT, "transfer timed out");
        let token = pending.token.clone();
        match msg {
            ClientMsg::AttachmentStart { files, .. } => {
                ensure!(!pending.batch && !pending.busy, "batch already started");
                ensure!(self.clients[&id].remote || pending.image, "local attachments use their original paths");
                if pending.image {
                    ensure!(
                        files.len() == 1 && files[0].size <= attachments::IMAGE_BYTES as u64,
                        "clipboard image exceeds 16 MiB or contains multiple files"
                    );
                }
                self.config.attachments.check(&files)?;
                let root = attachments::directory(&self.session);
                let settings = self.config.attachments.clone();
                let cancelled = pending.cancelled.clone();
                let batch_token = token.clone();
                self.attachment_job(id, token, move |batches| {
                    let result = (|| {
                        ensure!(!cancelled.load(Ordering::SeqCst), "attachment cancelled");
                        let reserved = batches
                            .values()
                            .map(|(b, _)| b.total.saturating_add(attachments::RESULT_RESERVE))
                            .fold(0u64, u64::saturating_add);
                        let batch = Batch::start(&root, &batch_token, &files, &settings, reserved)?;
                        batches.insert(batch_token, (batch, cancelled));
                        Ok(0)
                    })();
                    Completion::Ack(result)
                })?;
                let pending = self.clients.get_mut(&id).unwrap().attachments.pending.as_mut().unwrap();
                pending.batch = true;
                pending.busy = true;
            }
            ClientMsg::AttachmentChunk { file, offset, data, .. } => {
                ensure!(pending.batch && !pending.busy, "batch is not ready");
                ensure!(data.len() <= attachments::CHUNK.div_ceil(3) * 4, "attachment chunk too large");
                let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
                let batch_token = token.clone();
                self.attachment_job(id, token, move |batches| {
                    Completion::Ack((|| {
                        let (batch, _) = batches.get_mut(&batch_token).context("batch has not started")?;
                        batch.chunk(file, offset, &bytes)?;
                        Ok(batch.received)
                    })())
                })?;
                self.clients.get_mut(&id).unwrap().attachments.pending.as_mut().unwrap().busy = true;
            }
            ClientMsg::AttachmentFinish { .. } => {
                ensure!(pending.batch && !pending.busy, "batch is not ready");
                let batch_token = token.clone();
                self.attachment_job(id, token, move |batches| {
                    Completion::Finished((|| {
                        let (batch, _) = batches.remove(&batch_token).context("batch has not started")?;
                        batch.finish()
                    })())
                })?;
                self.clients.get_mut(&id).unwrap().attachments.pending.as_mut().unwrap().busy = true;
            }
            ClientMsg::AttachmentText { text, .. } => {
                ensure!(!pending.batch && text.len() <= 64 * 1024, "invalid text paste");
                let pane = pending.pane;
                self.buffer_paste(pane, std::str::from_utf8(&text)?).map_err(anyhow::Error::msg)?;
                let pending = self.clients.get_mut(&id).unwrap().attachments.pending.take().unwrap();
                let mode = self.paste_to(id, pane, &[pane], text);
                self.clients.get_mut(&id).unwrap().mode = mode;
                let same_focus = self.client_focus(id) == Some(pane);
                let kind =
                    if !same_focus && !pending.keys.is_empty() { NoticeKind::Warning } else { NoticeKind::Notice };
                let message = if !same_focus && !pending.keys.is_empty() {
                    self.clients.get_mut(&id).unwrap().attachments.blocked = Some(token.clone());
                    "Text submitted to the reserved pane. Focus changed: queued typing discarded. Press Ctrl+C, then select the pane before typing."
                } else {
                    "Text submitted to the reserved pane. No Enter added."
                };
                self.send_msg(id, &ServerMsg::AttachmentResult { kind, pasted: true, token, message: message.into() });
                if same_focus {
                    self.client_input(id, &pending.keys);
                }
            }
            ClientMsg::AttachmentLocal { paths, .. } => {
                ensure!(!self.clients[&id].remote && !pending.image, "this attachment must transfer its files");
                ensure!(!paths.is_empty() && paths.len() <= self.config.attachments.files, "invalid file count");
                ensure!(
                    paths.iter().all(|p| p.len() <= 4096
                        && std::path::Path::new(p).is_absolute()
                        && !p.chars().any(char::is_control)),
                    "invalid local attachment path"
                );
                let paths = paths.iter().map(|p| crate::shell::quote(p)).collect::<Vec<_>>().join(" ");
                let pane = pending.pane;
                self.buffer_paste(pane, &paths).map_err(anyhow::Error::msg)?;
                let pending = self.clients.get_mut(&id).unwrap().attachments.pending.take().unwrap();
                let mode = self.paste_to(id, pane, &[pane], paths.into_bytes());
                self.clients.get_mut(&id).unwrap().mode = mode;
                let same_focus = self.client_focus(id) == Some(pane);
                let kind =
                    if !same_focus && !pending.keys.is_empty() { NoticeKind::Warning } else { NoticeKind::Notice };
                let message = if !same_focus && !pending.keys.is_empty() {
                    self.clients.get_mut(&id).unwrap().attachments.blocked = Some(token.clone());
                    "Local paths submitted to the reserved pane. Focus changed: queued typing discarded. Press Ctrl+C, then select the pane before typing."
                } else {
                    "Local paths submitted to the reserved pane. No Enter added."
                };
                self.send_msg(id, &ServerMsg::AttachmentResult { kind, pasted: true, token, message: message.into() });
                if same_focus {
                    self.client_input(id, &pending.keys);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn attachment_job(
        &mut self,
        id: ClientId,
        token: String,
        work: impl FnOnce(&mut Batches) -> Completion + Send + 'static,
    ) -> Result<()> {
        if self.attachment_worker.is_none() {
            self.attachment_worker = Some(Worker::start(self.tx.clone())?);
        }
        self.attachment_worker
            .as_ref()
            .unwrap()
            .jobs
            .try_send(Job { id, token, work: Box::new(work) })
            .map_err(|_| anyhow::anyhow!("attachment storage is busy; try again"))
    }

    fn record_attachment(&mut self, id: ClientId, token: &str, message: &str, kind: NoticeKind) {
        let session = self.session.clone();
        let saved_token = token.to_string();
        let message = message.to_string();
        if let Err(error) = self.attachment_job(id, token.to_string(), move |_| {
            Completion::Recorded(attachments::record(&session, &saved_token, &message, kind))
        }) {
            self.log_message(&format!("attachment result could not be saved: {error:#}"), NoticeKind::Error);
        }
    }

    pub(super) fn attachment_complete(&mut self, id: ClientId, token: String, result: Completion) {
        let result = match result {
            Completion::Status(message, kind) => {
                // Older records have no kind; their message remains readable.
                let pasted = message.starts_with("Files received. Paths submitted to the reserved pane.");
                self.send_msg(id, &ServerMsg::AttachmentResult { kind, pasted, token, message });
                return;
            }
            Completion::Purged(result) => {
                match result {
                    Ok(_) => self.toast(
                        id,
                        NoticeKind::Notice,
                        "Attachments removed. Previously pasted paths no longer exist.",
                    ),
                    Err(e) => self.toast(
                        id,
                        NoticeKind::Error,
                        format!("Attachments could not be removed: {e}. Retry Purge attachments."),
                    ),
                }
                return;
            }
            Completion::Recorded(result) => {
                if let Err(error) = result {
                    self.log_message(&format!("attachment result could not be saved: {error:#}"), NoticeKind::Error);
                }
                return;
            }
            result => result,
        };
        let Some(pending) =
            self.clients.get(&id).and_then(|c| c.attachments.pending.as_ref()).filter(|p| p.token == token)
        else {
            return;
        };
        if pending.started.elapsed() >= attachments::TIMEOUT || !self.attachment_target(pending.pane) {
            self.attachment_failed(id, token, "the reserved pane closed or the attachment timed out");
            return;
        }
        match result {
            Completion::Ack(Ok(received)) => {
                self.clients.get_mut(&id).unwrap().attachments.pending.as_mut().unwrap().busy = false;
                self.send_msg(id, &ServerMsg::AttachmentAck { token, received });
            }
            Completion::Ack(Err(error)) | Completion::Finished(Err(error)) => {
                self.attachment_failed(id, token, &format!("{error:#}"));
            }
            Completion::Finished(Ok(paths)) => {
                let pending = self.clients.get_mut(&id).unwrap().attachments.pending.take().unwrap();
                if let Err(error) = self.buffer_paste(pending.pane, &paths) {
                    self.clients.get_mut(&id).unwrap().attachments.blocked = Some(token.clone());
                    let message = format!(
                        "Files kept, no paths or queued typing pasted: {error}. Press Ctrl+C, then use Attach files when the pane is ready."
                    );
                    self.record_attachment(id, &token, &message, NoticeKind::Error);
                    self.send_msg(
                        id,
                        &ServerMsg::AttachmentResult {
                            kind: NoticeKind::Error,
                            pasted: false,
                            token,
                            message: message.clone(),
                        },
                    );
                    self.toast(id, NoticeKind::Error, message);
                    return;
                }
                let mode = self.paste_to(id, pending.pane, &[pending.pane], paths.into_bytes());
                let same_focus = self.client_focus(id) == Some(pending.pane);
                let kind =
                    if !same_focus && !pending.keys.is_empty() { NoticeKind::Warning } else { NoticeKind::Notice };
                let message = if !same_focus && !pending.keys.is_empty() {
                    self.clients.get_mut(&id).unwrap().attachments.blocked = Some(token.clone());
                    "Files received. Paths submitted to the reserved pane. Focus changed: queued typing discarded. Press Ctrl+C, then select the pane before typing."
                } else {
                    "Files received. Paths submitted to the reserved pane. No Enter added."
                };
                self.clients.get_mut(&id).unwrap().mode = mode;
                self.record_attachment(id, &token, message, kind);
                self.send_msg(id, &ServerMsg::AttachmentResult { kind, pasted: true, token, message: message.into() });
                self.toast(id, kind, if !same_focus && !pending.keys.is_empty() {
                    "Attachment sent. Focus changed: queued typing discarded. Select the destination pane before typing."
                } else if pending.image { "Image sent." } else { "Files sent." });
                if same_focus {
                    self.client_input(id, &pending.keys);
                }
            }
            _ => unreachable!(),
        }
    }

    pub(super) fn expire_attachments(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .clients
            .values()
            .filter_map(|c| {
                let p = c.attachments.pending.as_ref()?;
                (now.duration_since(p.started) >= attachments::TIMEOUT || !self.attachment_target(p.pane))
                    .then(|| (c.id, p.token.clone()))
            })
            .collect();
        for (id, token) in expired {
            self.attachment_failed(id, token, "the reserved pane closed or the attachment timed out");
        }
    }

    pub(super) fn purge_attachments(&mut self, id: ClientId) {
        if self.clients.values().any(|c| c.attachments.pending.is_some()) {
            self.toast(id, NoticeKind::Warning, "Cancel active attachments before purging.");
            return;
        }
        let session = self.session.clone();
        if let Err(error) =
            self.attachment_job(id, String::new(), move |_| Completion::Purged(attachments::remove(&session)))
        {
            self.toast(
                id,
                NoticeKind::Error,
                format!("Attachments could not be removed: {error}. Retry Purge attachments."),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachments::FileSpec;
    use crate::server::{
        queue,
        tests::{client, server},
    };

    fn setup(name: &str) -> (Server, ClientId, std::os::unix::net::UnixStream, PaneId, queue::EventReceiver) {
        let _batch = crate::pane::batch();
        setup_command(name, vec!["true".into()])
    }

    fn setup_command(
        name: &str,
        command: Vec<String>,
    ) -> (Server, ClientId, std::os::unix::net::UnixStream, PaneId, queue::EventReceiver) {
        let (tx, rx) = queue::channel(4096);
        let mut server = server(tx);
        server.session = format!("attachment-{name}");
        let (id, peer) = client(&mut server);
        let ws = server.clients[&id].workspace;
        let pane = server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command, ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        let c = server.clients.get_mut(&id).unwrap();
        c.remote = true;
        c.attachments.negotiated = true;
        (server, id, peer, pane, rx)
    }

    fn wait_for_program(server: &Server, pane: PaneId, program: &str) {
        let start = Instant::now();
        while server.panes[&pane].foreground_argv().is_none_or(|argv| argv.first().is_none_or(|p| p != program)) {
            assert!(start.elapsed() < crate::testing::PATIENCE, "{program} never reached the foreground");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn settle(server: &mut Server, rx: &queue::EventReceiver) {
        // A completion may enqueue the durable result. A second barrier also waits for it.
        for _ in 0..2 {
            server.attachment_job(0, "barrier".into(), |_| Completion::Recorded(Ok(()))).unwrap();
            loop {
                let event = rx.recv_timeout(crate::testing::PATIENCE).unwrap();
                if let Event::Attachment(id, token, result) = event {
                    if token == "barrier" {
                        break;
                    }
                    server.attachment_complete(id, token, result);
                }
            }
        }
    }

    fn begin(server: &mut Server, id: ClientId, rx: &queue::EventReceiver) -> String {
        server.offer_attachments(id, None);
        let token = server.clients[&id].attachments.pending.as_ref().unwrap().token.clone();
        server.attachment_message(
            id,
            ClientMsg::AttachmentStart {
                token: token.clone(),
                files: vec![FileSpec { name: "a.txt".into(), size: 3 }],
            },
        );
        settle(server, rx);
        assert!(server.clients[&id].attachments.pending.as_ref().unwrap().batch);
        assert!(!server.clients[&id].attachments.pending.as_ref().unwrap().busy);
        token
    }

    fn chunk(server: &mut Server, id: ClientId, token: &str, rx: &queue::EventReceiver) {
        server.attachment_message(
            id,
            ClientMsg::AttachmentChunk { token: token.into(), file: 0, offset: 0, data: "YWJj".into() },
        );
        settle(server, rx);
    }

    #[test]
    fn a_shortcut_reserves_the_advertised_agent_or_refuses_before_reading() {
        let (mut server, id, _peer, pane, _rx) = setup_command("shortcut-target", vec!["cat".into()]);
        wait_for_program(&server, pane, "cat");
        server.clients.get_mut(&id).unwrap().attachments.images = true;
        let request =
            |pane| ClientMsg::AttachmentRequest { token: "ab".into(), text: Vec::new(), image: true, pane: Some(pane) };
        for phase in ["old", "shell", "wrong", "form", "agent"] {
            server.clients.get_mut(&id).unwrap().attachments.agent_clipboard = phase != "old";
            server.panes.get_mut(&pane).unwrap().agent.kind = (phase != "shell").then(|| "claude".into());
            server.clients.get_mut(&id).unwrap().mode = if phase == "form" {
                Mode::Prompt(super::super::ui::Prompt::new("Path", "", super::super::ui::PromptAction::OpenPreview))
            } else {
                Mode::Normal
            };
            server.attachment_message(id, request(if phase == "wrong" { pane + 100 } else { pane }));
            if phase == "agent" {
                assert_eq!(server.clients[&id].attachments.pending.as_ref().unwrap().pane, pane);
            } else {
                assert!(server.clients[&id].attachments.pending.is_none());
            }
        }
        assert!(!attachments::directory(&server.session).exists());
    }

    #[test]
    fn focus_updates_are_negotiated_and_follow_agent_mode_and_pane_changes() {
        let (mut server, id, _peer, pane, _events) = setup_command("agent-focus", vec!["cat".into()]);
        wait_for_program(&server, pane, "cat");
        assert_eq!(server.clipboard_focus(id), None, "older clients receive no new message");
        server.clients.get_mut(&id).unwrap().attachments.agent_clipboard = true;
        assert_eq!(server.clipboard_focus(id), Some((Some(pane), false)));
        for agent in ["claude", "codex"] {
            server.panes.get_mut(&pane).unwrap().agent.kind = Some(agent.into());
            assert_eq!(server.clipboard_focus(id), Some((Some(pane), true)));
            server.clients.get_mut(&id).unwrap().mode =
                Mode::Prompt(super::super::ui::Prompt::new("Path", "", super::super::ui::PromptAction::OpenPreview));
            assert_eq!(server.clipboard_focus(id), Some((Some(pane), false)));
            server.clients.get_mut(&id).unwrap().mode = Mode::Normal;
            assert_eq!(server.clipboard_focus(id), Some((Some(pane), true)));
            server.panes.get_mut(&pane).unwrap().agent.kind = None;
            assert_eq!(server.clipboard_focus(id), Some((Some(pane), false)));
        }
        server.panes.get_mut(&pane).unwrap().agent.kind = Some("claude".into());
        server.panes.get_mut(&pane).unwrap().exit_code = Some(Some(0));
        assert_eq!(server.clipboard_focus(id), Some((Some(pane), false)), "an exited agent is not eligible");
        let ws = server.clients[&id].workspace;
        let _batch = crate::pane::batch();
        let other = server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        assert_eq!(server.clipboard_focus(id), Some((Some(other), false)));
    }

    #[test]
    fn a_suspended_hooked_agent_refreshes_focus_and_cannot_reserve_the_shell() {
        let (mut server, id, _peer, pane, _events) = setup_command("suspended-agent", vec!["sh".into()]);
        wait_for_program(&server, pane, "sh");
        server.panes.get_mut(&pane).unwrap().write_input(b"sleep 60\n".to_vec());
        wait_for_program(&server, pane, "sleep");
        let agent = &mut server.panes.get_mut(&pane).unwrap().agent;
        agent.kind = Some("claude".into());
        agent.hooked = true;
        let client = server.clients.get_mut(&id).unwrap();
        client.attachments.agent_clipboard = true;
        client.attachments.images = true;
        assert_eq!(server.clipboard_focus(id), Some((Some(pane), true)));
        server.render_clients();
        assert_eq!(server.clients[&id].attachments.focus, Some((Some(pane), true)));

        server.panes.get_mut(&pane).unwrap().write_input(vec![0x1a]);
        wait_for_program(&server, pane, "sh");
        server.clients.get_mut(&id).unwrap().dirty = super::super::Dirty::No;
        server.tick(Instant::now());
        assert_eq!(server.panes[&pane].agent.kind.as_deref(), Some("claude"));
        assert!(server.panes[&pane].agent.hooked);
        assert_eq!(server.clipboard_focus(id), Some((Some(pane), false)));
        assert_eq!(server.clients[&id].dirty, super::super::Dirty::Yes);
        server.render_clients();
        assert_eq!(server.clients[&id].attachments.focus, Some((Some(pane), false)));
        server.attachment_message(
            id,
            ClientMsg::AttachmentRequest { token: "ab".into(), text: vec![0x16], image: true, pane: Some(pane) },
        );
        assert!(server.clients[&id].attachments.pending.is_none());
        assert!(server.clients[&id].attachments.blocked.is_none());
        assert!(!attachments::directory(&server.session).exists());

        // Ctrl+V in the shell quotes the next byte; finish that quoted input before resuming.
        server.panes.get_mut(&pane).unwrap().write_input(b"\nfg\n".to_vec());
        wait_for_program(&server, pane, "sleep");
        server.clients.get_mut(&id).unwrap().dirty = super::super::Dirty::No;
        server.tick(Instant::now());
        assert_eq!(server.clients[&id].dirty, super::super::Dirty::Yes);
        assert_eq!(server.clipboard_focus(id), Some((Some(pane), true)));
    }

    #[test]
    fn stale_shortcut_requests_replay_the_key_in_copy_mode_before_following_input() {
        for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
            let (mut server, id, mut peer, pane, _events) = setup_command("shortcut-copy", vec!["cat".into()]);
            wait_for_program(&server, pane, "cat");
            server.clients.get_mut(&id).unwrap().attachments.agent_clipboard = true;
            server.clients.get_mut(&id).unwrap().attachments.images = true;
            server.panes.get_mut(&pane).unwrap().agent.kind = Some("claude".into());
            assert_eq!(server.clipboard_focus(id), Some((Some(pane), true)));
            server.client_input(id, b"\x02v");
            assert!(server.panes[&pane].in_copy_mode());
            server.attachment_message(
                id,
                ClientMsg::AttachmentRequest { token: "ab".into(), text: key.to_vec(), image: true, pane: Some(pane) },
            );
            let term = server.panes[&pane].term().unwrap();
            assert_eq!(term.term.selection.as_ref().unwrap().ty, alacritty_terminal::selection::SelectionType::Block);
            let before = term.term.vi_mode_cursor.point;
            server.client_input(id, b"l");
            assert_eq!(server.panes[&pane].term().unwrap().term.vi_mode_cursor.point.column.0, before.column.0 + 1);
            assert!(server.clients[&id].attachments.pending.is_none());
            assert!(server.clients[&id].attachments.blocked.is_none());
            assert!(server.clients[&id].toast.is_none());
            let (reply, received) = crossbeam_channel::bounded(1);
            std::thread::spawn(move || {
                // Entering and leaving the leader menu updates the terminal's keyboard mode.
                for bytes in [b"\x1b[=29;1u\x1b[?u".as_slice(), b"\x1b[=5;1u"] {
                    assert_eq!(
                        crate::protocol::read_frame(&mut peer).unwrap().unwrap(),
                        (crate::protocol::BYTES, bytes.to_vec())
                    );
                }
                let _ = reply.send(crate::protocol::read_frame(&mut peer));
            });
            let (kind, bytes) = received.recv_timeout(crate::testing::PATIENCE).unwrap().unwrap().unwrap();
            assert_eq!(kind, crate::protocol::JSON);
            let ServerMsg::AttachmentResult { token, pasted, message, kind } = serde_json::from_slice(&bytes).unwrap()
            else {
                panic!("the shortcut must acknowledge its fallback");
            };
            assert_eq!(token, "ab");
            assert!(pasted);
            assert!(message.is_empty());
            assert_eq!(kind, NoticeKind::Notice);
        }
    }

    #[test]
    fn image_offers_require_capability_and_accept_private_storage_in_local_sessions() {
        let (mut server, id, _peer, pane, rx) = setup("image-local");
        server.offer_attachment(id, None, true);
        assert!(server.clients[&id].attachments.pending.is_none());
        assert!(server.clients[&id].toast.as_ref().unwrap().0.contains("Update kraal on your laptop"));
        server.clients.get_mut(&id).unwrap().attachments.images = true;
        server.clients.get_mut(&id).unwrap().remote = false;
        server.offer_attachment(id, None, true);
        let pending = server.clients[&id].attachments.pending.as_ref().unwrap();
        assert!(pending.image);
        assert_eq!(pending.pane, pane);
        let token = pending.token.clone();
        assert!(
            server
                .receive_attachment(
                    id,
                    ClientMsg::AttachmentLocal { token: token.clone(), paths: vec!["/tmp/image.png".into()] }
                )
                .is_err()
        );
        for files in [
            vec![FileSpec { name: "clipboard.png".into(), size: attachments::IMAGE_BYTES as u64 + 1 }],
            vec![FileSpec { name: "clipboard.png".into(), size: 1 }; 2],
        ] {
            assert!(server.receive_attachment(id, ClientMsg::AttachmentStart { token: token.clone(), files }).is_err());
        }
        server
            .receive_attachment(
                id,
                ClientMsg::AttachmentStart {
                    token: token.clone(),
                    files: vec![FileSpec { name: "clipboard.png".into(), size: attachments::IMAGE_BYTES as u64 }],
                },
            )
            .unwrap();
        settle(&mut server, &rx);
        let root = attachments::directory(&server.session);
        assert!(root.join(format!(".{token}/0/clipboard.png")).exists());
        server.attachment_message(id, ClientMsg::AttachmentCancel { token });
        settle(&mut server, &rx);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        attachments::remove(&server.session).unwrap();
    }

    #[test]
    fn image_retry_reserves_its_kind_and_completes_locally() {
        let (mut server, id, _peer, pane, rx) = setup("image-retry");
        server.clients.get_mut(&id).unwrap().remote = false;
        let request = || ClientMsg::AttachmentRequest { token: "ab".into(), text: Vec::new(), image: true, pane: None };
        server.attachment_message(id, request());
        assert!(server.clients[&id].attachments.pending.is_none(), "images need their capability");
        server.clients.get_mut(&id).unwrap().attachments.images = true;
        server.offer_attachment(id, None, true);
        let token = server.clients[&id].attachments.pending.as_ref().unwrap().token.clone();
        server.expire_attachments(Instant::now() + attachments::TIMEOUT);
        assert_eq!(server.clients[&id].attachments.blocked.as_ref(), Some(&token));
        server.attachment_message(id, ClientMsg::AttachmentDismiss { token });
        server.attachment_message(id, request());
        let pending = server.clients[&id].attachments.pending.as_ref().unwrap();
        assert!(pending.image);
        assert_eq!(pending.pane, pane);
        server.attachment_message(
            id,
            ClientMsg::AttachmentStart {
                token: "ab".into(),
                files: vec![FileSpec { name: "clipboard.png".into(), size: 3 }],
            },
        );
        settle(&mut server, &rx);
        assert!(server.clients[&id].attachments.pending.as_ref().unwrap().batch);
        chunk(&mut server, id, "ab", &rx);
        server.attachment_message(id, ClientMsg::AttachmentFinish { token: "ab".into() });
        settle(&mut server, &rx);
        assert!(server.clients[&id].attachments.pending.is_none());
        assert!(server.clients[&id].attachments.blocked.is_none());
        assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, "Image sent.");
        assert_eq!(std::fs::read(attachments::directory(&server.session).join("ab/0/clipboard.png")).unwrap(), b"abc");
        attachments::remove(&server.session).unwrap();
    }

    #[test]
    fn pasted_path_reservation_needs_negotiation_and_text_keeps_the_original_target() {
        let (mut server, id, _peer, pane, _rx) = setup("pasted-text");
        let original = b"  '/laptop/notes client.txt'  \n";
        let request =
            || ClientMsg::AttachmentRequest { token: "ab".into(), text: original.to_vec(), image: false, pane: None };
        server.attachment_message(id, request());
        assert!(server.clients[&id].attachments.pending.is_none());
        server.clients.get_mut(&id).unwrap().attachments.recognition = true;
        server.attachment_message(id, request());
        assert_eq!(server.clients[&id].attachments.pending.as_ref().unwrap().pane, pane);
        assert!(!attachments::directory(&server.session).exists());
        let ws = server.clients[&id].workspace;
        let _batch = crate::pane::batch();
        let other = server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        assert_eq!(server.client_focus(id), Some(other));
        server.panes.get_mut(&pane).unwrap().ssh =
            Some(crate::servers::Server { name: "prod".into(), host: "prod".into(), ..Default::default() });
        server.attachment_message(id, ClientMsg::AttachmentText { token: "ab".into(), text: original.to_vec() });
        assert!(server.clients[&id].attachments.pending.is_none());
        assert!(matches!(&server.clients[&id].mode, Mode::Confirm(confirm)
            if matches!(&confirm.action, super::super::ui::ConfirmAction::Production { pending, .. }
                if pending.targets.len() == 1 && pending.targets[0].0 == pane
                    && matches!(pending.items.as_slice(), [super::super::ui::InputItem::Paste(text)] if text == original))));
        assert!(!attachments::directory(&server.session).exists());
    }

    #[test]
    fn client_splitter_reserves_text_and_files_before_focus_changes() {
        use crate::protocol;
        use std::sync::Mutex;
        struct Wire(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Wire {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for files in [false, true] {
            let (mut server, id, _peer, pane, _rx) = setup(if files { "split-file" } else { "split-text" });
            server.clients.get_mut(&id).unwrap().attachments.recognition = true;
            let wire = Arc::new(Mutex::new(Vec::new()));
            let frame = || {
                let mut bytes = std::io::Cursor::new(std::mem::take(&mut *wire.lock().unwrap()));
                let frame = protocol::read_frame(&mut bytes).unwrap().unwrap();
                assert_eq!(bytes.position(), bytes.get_ref().len() as u64);
                frame
            };
            let writer: Box<dyn std::io::Write + Send> = Box::new(Wire(wire.clone()));
            let client = crate::client::AttachmentController::new(Arc::new(Mutex::new(Some(writer))), true);
            client.negotiate(true);
            let path = std::env::temp_dir().join(format!("kraal-split-target-{}", std::process::id()));
            let text = if files {
                std::fs::write(&path, b"abc").unwrap();
                format!("'{}'\n", path.display()).into_bytes()
            } else {
                b"ordinary text\n".to_vec()
            };
            client.input(&[b"\x1b[200~".as_slice(), &text[..4]].concat());
            let (kind, bytes) = frame();
            assert!(matches!(serde_json::from_slice::<ClientMsg>(&bytes).unwrap(), ClientMsg::PasteStart));
            server.client_frame(id, kind, bytes);
            assert!(matches!(&server.clients[&id].paste,
                Some(super::super::input::PasteFlow::Held { focused, targets, .. })
                    if *focused == pane && targets == &[pane]));
            let ws = server.clients[&id].workspace;
            let _batch = crate::pane::batch();
            let other = server
                .spawn_pane_in(
                    ws,
                    None,
                    crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() },
                    protocol::Placement::Tab,
                    None,
                )
                .unwrap();
            assert_eq!(server.client_focus(id), Some(other));
            server.panes.get_mut(&pane).unwrap().ssh =
                Some(crate::servers::Server { name: "prod".into(), host: "prod".into(), ..Default::default() });
            client.input(&[&text[4..], b"\x1b[201~"].concat());
            let (kind, bytes) = frame();
            server.client_frame(id, kind, bytes);
            assert!(server.clients[&id].paste.is_none());
            if files {
                let pending = server.clients[&id].attachments.pending.as_ref().unwrap();
                assert_eq!(pending.pane, pane);
                let token = pending.token.clone();
                server.attachment_message(id, ClientMsg::AttachmentText { token, text: text.clone() });
                std::fs::remove_file(path).unwrap();
            }
            assert!(matches!(&server.clients[&id].mode, Mode::Confirm(confirm)
                if matches!(&confirm.action, super::super::ui::ConfirmAction::Production { pending, .. }
                    if pending.targets.len() == 1 && pending.targets[0].0 == pane
                        && matches!(pending.items.as_slice(), [super::super::ui::InputItem::Paste(data)] if data == &text))));
        }
    }

    #[test]
    fn a_local_session_pastes_paths_unchanged_even_in_an_ssh_pane() {
        let (mut server, id, _peer, pane, _rx) = setup("local-paste");
        server.clients.get_mut(&id).unwrap().remote = false;
        server.panes.get_mut(&pane).unwrap().ssh =
            Some(crate::servers::Server { name: "dev".into(), host: "dev".into(), ..Default::default() });
        let path = std::env::temp_dir().join(format!("kraal-local-path-{}", std::process::id()));
        std::fs::write(&path, b"local").unwrap();
        server.client_input(id, format!("\x1b[200~'{}'\x1b[201~", path.display()).as_bytes());
        assert!(matches!(server.clients[&id].mode, Mode::Normal));
        assert!(server.clients[&id].attachments.pending.is_none());
        assert!(!attachments::directory(&server.session).exists());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pasted_paths_in_an_open_form_remain_text() {
        let (mut server, id, _peer, _, _rx) = setup("pasted-form");
        server.clients.get_mut(&id).unwrap().attachments.recognition = true;
        server.clients.get_mut(&id).unwrap().mode =
            Mode::Prompt(super::super::ui::Prompt::new("Path", "", super::super::ui::PromptAction::OpenPreview));
        server.attachment_message(
            id,
            ClientMsg::AttachmentRequest { token: "ab".into(), text: b"/laptop/a".to_vec(), image: false, pane: None },
        );
        assert!(server.clients[&id].attachments.pending.is_none());
        assert!(matches!(&server.clients[&id].mode, Mode::Prompt(prompt) if prompt.value == "/laptop/a"));
    }

    #[test]
    fn attachment_action_holds_the_same_input_in_combined_and_split_frames() {
        for split in [false, true] {
            let (mut server, id, _peer, _, _rx) = setup(if split { "split" } else { "combined" });
            let action = b"\x02 Attach files\r";
            let tail = b"/tmp/example\r";
            if split {
                server.client_frame(id, crate::protocol::BYTES, action.to_vec());
                server.client_frame(id, crate::protocol::BYTES, tail.to_vec());
            } else {
                server.client_frame(id, crate::protocol::BYTES, [action.as_slice(), tail].concat());
            }
            let pending = server.clients[&id].attachments.pending.as_ref().unwrap();
            assert_eq!(pending.keys, tail);
            let token = pending.token.clone();
            server.attachment_message(id, ClientMsg::AttachmentCancel { token });
            assert!(server.clients[&id].attachments.pending.is_none());
            assert!(server.clients[&id].attachments.blocked.is_some());
        }
    }

    #[test]
    fn stalled_write_and_flush_leave_other_clients_and_cancellation_responsive() {
        for flush in [false, true] {
            let (mut server, id, _peer, _, rx) = setup(if flush { "stalled-flush" } else { "stalled-write" });
            let token = begin(&mut server, id, &rx);
            if flush {
                chunk(&mut server, id, &token, &rx);
            }
            let (other, _peer2) = client(&mut server);
            let (entered, waiting) = crossbeam_channel::bounded(1);
            let (resume, resumed) = crossbeam_channel::bounded(1);
            *server.attachment_worker.as_ref().unwrap().pause.lock().unwrap() = Some((entered, resumed));
            server.attachment_message(
                id,
                if flush {
                    ClientMsg::AttachmentFinish { token: token.clone() }
                } else {
                    ClientMsg::AttachmentChunk { token: token.clone(), file: 0, offset: 0, data: "YWJj".into() }
                },
            );
            waiting.recv_timeout(crate::testing::PATIENCE).unwrap();
            assert!(server.clients[&id].attachments.pending.as_ref().unwrap().busy);
            assert!(!attachments::directory(&server.session).join(&token).exists());
            server.client_frame(other, crate::protocol::BYTES, b"\x02 ".to_vec());
            assert!(matches!(server.clients[&other].mode, Mode::Picker(_)));
            assert!(server.attachment_input(id, b"must not reach the pane\r"));
            if flush {
                server.expire_attachments(Instant::now() + attachments::TIMEOUT);
            } else {
                server.attachment_message(id, ClientMsg::AttachmentCancel { token: token.clone() });
            }
            assert!(server.clients[&id].attachments.pending.is_none());
            assert!(server.clients[&id].attachments.blocked.is_some());
            let message = server.clients[&id].toast.as_ref().unwrap().0.clone();
            resume.send(()).unwrap();
            settle(&mut server, &rx);
            assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, message);
            assert!(server.clients[&id].attachments.blocked.is_some());
            assert!(!attachments::directory(&server.session).join(format!(".{token}")).exists());
            attachments::remove(&server.session).unwrap();
        }
    }

    #[test]
    fn reservation_is_connection_bound_and_duplicate_finish_only_reports_the_result() {
        let (mut server, id, _peer, pane, rx) = setup("finish");
        let token = begin(&mut server, id, &rx);
        let (other, _peer2) = client(&mut server);
        server.clients.get_mut(&other).unwrap().attachments.negotiated = true;
        server.attachment_message(other, ClientMsg::AttachmentCancel { token: token.clone() });
        assert!(server.clients[&id].attachments.pending.is_some());
        assert_eq!(server.clients[&id].attachments.pending.as_ref().unwrap().pane, pane);
        chunk(&mut server, id, &token, &rx);
        assert!(!attachments::directory(&server.session).join(&token).exists());
        server.attachment_message(id, ClientMsg::AttachmentFinish { token: token.clone() });
        settle(&mut server, &rx);
        assert!(server.clients[&id].attachments.pending.is_none());
        assert_eq!(
            std::fs::read(attachments::directory(&server.session).join(&token).join("0/a.txt")).unwrap(),
            b"abc"
        );
        let result = attachments::result(&server.session, &token).unwrap();
        assert!(result.0.contains("reserved pane"));
        server.clients.get_mut(&id).unwrap().toast = None;
        server.attachment_message(id, ClientMsg::AttachmentFinish { token: token.clone() });
        settle(&mut server, &rx);
        assert!(server.clients[&id].toast.is_none());
        server.drop_client(id);
        assert_eq!(attachments::result(&server.session, &token).unwrap(), result);
        attachments::remove(&server.session).unwrap();
    }

    #[test]
    fn cancel_disconnect_timeout_and_closed_pane_remove_partial_batches() {
        for cause in ["cancel", "disconnect", "timeout", "closed", "overflow", "invalid", "incomplete"] {
            let (mut server, id, _peer, pane, rx) = setup(cause);
            let token = begin(&mut server, id, &rx);
            assert!(server.attachment_input(id, b"echo later\r"));
            let root = attachments::directory(&server.session);
            match cause {
                "cancel" => server.attachment_message(id, ClientMsg::AttachmentCancel { token: token.clone() }),
                "disconnect" => server.drop_client(id),
                "timeout" => server.expire_attachments(Instant::now() + attachments::TIMEOUT),
                "closed" => {
                    server.panes.get_mut(&pane).unwrap().exit_code = Some(Some(0));
                    server.expire_attachments(Instant::now());
                }
                "overflow" => {
                    assert!(server.attachment_input(id, &vec![b'x'; 64 * 1024]));
                }
                "invalid" => server.attachment_message(
                    id,
                    ClientMsg::AttachmentChunk { token: token.clone(), file: 0, offset: 1, data: "YQ==".into() },
                ),
                "incomplete" => server.attachment_message(id, ClientMsg::AttachmentFinish { token: token.clone() }),
                _ => unreachable!(),
            }
            settle(&mut server, &rx);
            assert!(!root.join(format!(".{token}")).exists(), "{cause}");
            assert!(!root.join(&token).exists(), "{cause}");
            if let Some(c) = server.clients.get(&id) {
                assert!(c.attachments.pending.is_none());
                assert!(c.toast.as_ref().unwrap().0.contains("No paths or queued typing pasted"));
            }
            if cause != "disconnect" {
                assert!(server.attachment_input(id, b"tail of a paste\x1b[201~\r"));
                server.attachment_message(id, ClientMsg::AttachmentDismiss { token: "wrong".into() });
                assert!(server.attachment_input(id, b"still blocked"));
                server.attachment_message(id, ClientMsg::AttachmentDismiss { token: token.clone() });
                assert!(!server.attachment_input(id, b"new input"));
            }
            attachments::remove(&server.session).unwrap();
        }
    }

    #[test]
    fn negotiation_and_local_paths_do_not_create_storage() {
        let (mut server, id, _peer, _, _rx) = setup("local");
        server.clients.get_mut(&id).unwrap().attachments.negotiated = false;
        server.offer_attachments(id, None);
        assert!(server.clients[&id].attachments.pending.is_none());
        assert!(server.clients[&id].toast.as_ref().unwrap().0.contains("Update kraal"));
        server.clients.get_mut(&id).unwrap().attachments.negotiated = true;
        server.clients.get_mut(&id).unwrap().remote = false;
        server.offer_attachments(id, None);
        let token = server.clients[&id].attachments.pending.as_ref().unwrap().token.clone();
        server.attachment_message(
            id,
            ClientMsg::AttachmentLocal { token, paths: vec!["/local/notes client.txt".into()] },
        );
        assert!(server.clients[&id].attachments.pending.is_none());
        assert!(!attachments::directory(&server.session).exists());
    }

    #[test]
    fn focus_changes_keep_the_reserved_target_and_purge_refuses_live_transfers() {
        let (mut server, id, _peer, pane, rx) = setup("focus");
        let token = begin(&mut server, id, &rx);
        let ws = server.clients[&id].workspace;
        let _batch = crate::pane::batch();
        let other = server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        assert_eq!(server.client_focus(id), Some(other));
        assert_eq!(server.clients[&id].attachments.pending.as_ref().unwrap().pane, pane);
        server.purge_attachments(id);
        assert!(server.clients[&id].attachments.pending.is_some());
        assert!(server.attachment_input(id, b"later"));
        chunk(&mut server, id, &token, &rx);
        server.attachment_message(id, ClientMsg::AttachmentFinish { token: token.clone() });
        settle(&mut server, &rx);
        assert!(attachments::result(&server.session, &token).unwrap().0.contains("reserved pane"));
        assert!(server.clients[&id].toast.as_ref().unwrap().0.contains("queued typing discarded"));
        assert!(server.attachment_input(id, b"\x03"));
        server.attachment_message(id, ClientMsg::AttachmentDismiss { token });
        assert!(!server.attachment_input(id, b"\x03"));
        server.purge_attachments(id);
        settle(&mut server, &rx);
        assert!(!attachments::directory(&server.session).exists());
    }
    #[test]
    fn broadcast_requires_confirmation_and_still_reserves_only_one_pane() {
        let (mut server, id, _peer, pane, _rx) = setup("broadcast");
        let ws = server.clients[&id].workspace;
        let _batch = crate::pane::batch();
        server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        let wi = server.ws_index(ws).unwrap();
        server.workspaces[wi].sync = crate::server::SyncScope::Workspace;
        server.clients.get_mut(&id).unwrap().attachments.images = true;
        for image in [false, true] {
            let mode = server.offer_attachment(id, None, image);
            let Mode::Confirm(confirm) = mode else { panic!("broadcast did not ask") };
            assert!(match confirm.action {
                crate::server::ui::ConfirmAction::AttachFiles(_) => !image,
                crate::server::ui::ConfirmAction::AttachClipboardImage(_) => image,
                _ => false,
            });
            assert!(server.clients[&id].attachments.pending.is_none());
        }
        assert!(matches!(server.offer_attachments(id, Some(pane)), Mode::Normal));
        assert_eq!(server.clients[&id].attachments.pending.as_ref().unwrap().pane, pane);
    }
}
