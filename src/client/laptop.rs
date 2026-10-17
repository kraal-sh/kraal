//! Local consent, directory listings and copies for a remote session's file manager.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::files::Side;
use crate::input::InputParser;
use crate::laptop::copy::{Operation, ToClient};
use crate::laptop::{Identity, Scope};
use crate::layout::PaneId;

mod connection;
mod input;
mod transfer;
mod view;

type Writer = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// Requests waiting behind a question or a transfer, and the path bytes they may hold.
const QUEUED: usize = 16;
const QUEUED_BYTES: usize = 64 << 20;

#[derive(Clone)]
struct Request {
    to_laptop: bool,
    operation: Operation,
    copy: Option<(String, Vec<String>)>,
    pane: PaneId,
    side: Side,
    request: u64,
    path: String,
}

struct Prompt {
    scroll: usize,
    replacement: Option<(Vec<String>, crossbeam_channel::Sender<bool>)>,
    request: Request,
    scope: Scope,
    selected: usize,
}

#[derive(Default)]
struct State {
    prompt: Option<Prompt>,
    allowed: bool,
    generation: u64,
    busy: bool,
    parser: InputParser,
    queued: VecDeque<Request>,
    copies: HashMap<String, CopyWorker>,
    notice: Option<(Instant, String)>,
    ack: Option<(PaneId, Side, u64, crossbeam_channel::Sender<()>)>,
}

struct CopyWorker {
    operation: Operation,
    control: crossbeam_channel::Sender<ToClient>,
    cancelled: Arc<AtomicBool>,
    count: usize,
    destination: Option<String>,
    completed: bool,
}

pub struct Controller {
    state: Mutex<State>,
    writer: Writer,
    destination: Option<(Vec<String>, String)>,
    identity: Option<Identity>,
    permissions: PathBuf,
}

fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

impl Controller {
    pub fn new(writer: Writer, target: &super::Target, identity: Option<Identity>) -> Arc<Self> {
        let destination = match target {
            super::Target::Remote { ssh, label, .. } => Some((ssh.clone(), clean(label))),
            super::Target::Local { .. } => None,
        };
        Arc::new(Self {
            state: Mutex::new(State::default()),
            writer,
            destination,
            identity,
            permissions: crate::paths::config_dir().join("laptop-files.toml"),
        })
    }
}

#[cfg(test)]
mod tests;
