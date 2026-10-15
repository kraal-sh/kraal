//! The server can offer a form, but only local keyboard input authorizes opening files.

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ratatui::layout::Rect;

use crate::attachments;
use crate::client::clipboard_image::Notices;
use crate::client::paste;
use crate::input::InputParser;
use crate::theme::NoticeKind;

mod connection;
mod input;
mod transfer;
mod view;

pub(super) use view::View;

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

#[derive(Clone)]
pub(super) struct TransferProgress {
    label: String,
    received: u64,
    total: u64,
    marker: char,
    ascii: bool,
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

struct ClipboardRead {
    key: Vec<u8>,
    focus: Option<(Option<crate::layout::PaneId>, bool)>,
    result: Option<Result<Vec<u8>, String>>,
    cancelled: Arc<AtomicBool>,
    queued: Vec<u8>,
    parser: InputParser,
}

#[derive(Default)]
struct State {
    clipboard_read: Option<ClipboardRead>,
    clipboard_limit: Option<usize>,
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
    pub fn set_clipboard_limit(&self, limit: usize) {
        self.state.lock().unwrap().clipboard_limit = Some(limit);
    }

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
}

#[cfg(test)]
mod tests;
