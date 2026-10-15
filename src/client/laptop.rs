//! Local consent, directory listings and copies for a remote session's file manager.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::laptop::copy::{self, FromClient, Operation, Packet, ToClient, Window};
use anyhow::{Context, Result, bail, ensure};

use ratatui::layout::Rect;

use crate::files::Side;
use crate::input::{InputEvent, InputParser, KeyCode};
use crate::laptop::{Identity, Listing, Scope};
use crate::layout::PaneId;
use crate::protocol::{self, ClientMsg};
use crate::theme::NoticeKind;

use super::attachments::View;
use super::paste;

type Writer = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

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

    fn send(&self, message: &ClientMsg) {
        if let Ok(mut writer) = self.writer.lock()
            && let Some(writer) = writer.as_mut()
        {
            let _ = protocol::write_json(writer, message);
        }
    }

    fn reply(&self, r: Request, result: Listing) {
        if let Some((token, _)) = r.copy {
            self.send(&ClientMsg::LaptopCopy { token, message: FromClient::Finish(result.map(|_| ())) });
        } else {
            self.send(&ClientMsg::LaptopListed { pane: r.pane, side: r.side, request: r.request, result });
        }
    }

    pub fn request(self: &Arc<Self>, pane: PaneId, side: Side, request: u64, path: String) {
        let request = Request { to_laptop: false, operation: Operation::Copy, copy: None, pane, side, request, path };
        self.state.lock().unwrap().queued.push_back(request);
        self.next();
    }

    fn next(self: &Arc<Self>) {
        loop {
            let mut state = self.state.lock().unwrap();
            if state.busy || state.prompt.is_some() {
                return;
            }
            let Some(request) = state.queued.pop_front() else { return };
            let retry = if request.copy.is_some() { "retry the copy" } else { "press r" };
            let scope = crate::config::Config::load()
                .map(|config| config.laptop_files.scope)
                .map_err(|e| {
                    format!("Cannot read laptop configuration: {e}. Fix config.toml on the laptop and {retry}")
                })
                .and_then(|scope| {
                    if self.destination.is_none() {
                        Err("Laptop files require attach --host".into())
                    } else if scope == Scope::Off {
                        Err(format!("Laptop files are off: change laptop_files.scope on the laptop and {retry}"))
                    } else {
                        Ok(scope)
                    }
                });
            let scope = match scope {
                Ok(scope) => scope,
                Err(error) => {
                    drop(state);
                    self.reply(request, Err(error));
                    continue;
                }
            };
            let (ssh, _) = self.destination.as_ref().unwrap();
            let writing = request.to_laptop;
            let remembered = if writing {
                paste::laptop_write_allowed(&self.permissions, ssh)
            } else {
                paste::laptop_allowed(&self.permissions, ssh)
            };
            if state.allowed || remembered {
                state.busy = true;
                let generation = state.generation;
                drop(state);
                self.start(request, scope, generation);
            } else {
                state.prompt = Some(Prompt { scroll: 0, replacement: None, request, scope, selected: 0 });
                drop(state);
                self.draw();
            }
            return;
        }
    }

    fn start(self: &Arc<Self>, request: Request, scope: Scope, generation: u64) {
        if request.copy.is_some() {
            if request.to_laptop {
                self.start_write(request, scope, generation);
            } else {
                self.start_copy(request, scope, generation);
            }
            return;
        }
        let this = self.clone();
        let retry = request.clone();
        let spawned = crate::threads::named("laptop-list").spawn(move || {
            let result =
                dirs::home_dir().ok_or_else(|| "Cannot find the laptop home directory".to_string()).and_then(|home| {
                    let sensitivity = crate::laptop::listing_case_sensitive(&request.path, &home, scope);
                    if let Ok(case_sensitive) = sensitivity {
                        this.send_current(
                            generation,
                            &ClientMsg::LaptopCaseSensitive {
                                pane: request.pane,
                                side: request.side,
                                request: request.request,
                                case_sensitive,
                            },
                        );
                    }
                    sensitivity
                        .and_then(|_| crate::laptop::list(&request.path, &home, scope, cfg!(target_os = "macos")))
                        .map_err(|e| format!("{e:#}. Press o to choose a directory"))
                });
            this.finish(generation, request, result);
        });
        if let Err(e) = spawned {
            self.finish(generation, retry, Err(format!("Cannot start laptop listing: {e}. Press r to retry")));
        }
    }

    fn finish(self: &Arc<Self>, generation: u64, request: Request, result: Listing) {
        // Serialize without holding the connection writer. Only one bounded frame is in
        // flight: its acknowledgement gives keyboard and resize traffic room on slow SSH.
        let bytes = serde_json::to_vec(&result).expect("laptop listing");
        if bytes.len() > protocol::LAPTOP_LIST_LIMIT {
            self.send_current(
                generation,
                &ClientMsg::LaptopListed {
                    pane: request.pane,
                    side: request.side,
                    request: request.request,
                    result: Err("Laptop listing is too large: choose a smaller directory".into()),
                },
            );
        } else if bytes.len() <= protocol::LAPTOP_CHUNK {
            self.send_current(
                generation,
                &ClientMsg::LaptopListed { pane: request.pane, side: request.side, request: request.request, result },
            );
        } else {
            let (ack, received) = crossbeam_channel::bounded(1);
            {
                let mut state = self.state.lock().unwrap();
                if state.generation != generation {
                    return;
                }
                state.ack = Some((request.pane, request.side, request.request, ack));
            }
            for (index, chunk) in bytes.chunks(protocol::LAPTOP_CHUNK).enumerate() {
                let done = (index + 1) * protocol::LAPTOP_CHUNK >= bytes.len();
                if !self.send_current(
                    generation,
                    &ClientMsg::LaptopListChunk {
                        pane: request.pane,
                        side: request.side,
                        request: request.request,
                        data: chunk.to_vec(),
                        done,
                    },
                ) {
                    break;
                }
                if received.recv_timeout(Duration::from_secs(2)).is_err() {
                    self.send_current(
                        generation,
                        &ClientMsg::LaptopListed {
                            pane: request.pane,
                            side: request.side,
                            request: request.request,
                            result: Err("Laptop listing stalled: press r to retry".into()),
                        },
                    );
                    break;
                }
            }
        }
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return;
        }
        state.ack = None;
        state.busy = false;
        drop(state);
        self.next();
    }

    fn send_current(&self, generation: u64, message: &ClientMsg) -> bool {
        let mut writer = self.writer.lock().unwrap();
        if self.state.lock().unwrap().generation != generation {
            return false;
        }
        let Some(connection) = writer.as_mut() else {
            return false;
        };
        if protocol::write_json(connection, message).is_err() {
            // A partially written frame must never be followed by another frame.
            *writer = None;
            return false;
        }
        true
    }

    pub fn acknowledge(&self, pane: PaneId, side: Side, request: u64) {
        let state = self.state.lock().unwrap();
        if let Some((p, s, r, ack)) = &state.ack
            && (*p, *s, *r) == (pane, side, request)
        {
            let _ = ack.try_send(());
        }
    }

    pub fn copy_message(self: &Arc<Self>, token: String, message: ToClient) {
        match message {
            ToClient::Start { .. } | ToClient::Write(_) | ToClient::Move(_) | ToClient::Delete { .. } => {
                let (sources, path, operation, to_laptop) = match message {
                    ToClient::Start { sources } => (sources, String::new(), Operation::Copy, false),
                    ToClient::Move(spec) => {
                        (spec.sources, spec.destination.to_string_lossy().into_owned(), Operation::Move, false)
                    }
                    ToClient::Delete { sources } => (sources, String::new(), Operation::Delete, false),
                    ToClient::Write(spec) => {
                        if spec.destination.as_os_str().is_empty() {
                            self.send(&ClientMsg::LaptopCopy {
                                token,
                                message: FromClient::Finish(Err(
                                    "Choose a laptop destination and retry the copy".into()
                                )),
                            });
                            return;
                        }
                        (spec.sources, spec.destination.to_string_lossy().into_owned(), spec.operation, true)
                    }
                    _ => unreachable!(),
                };

                if !crate::attachments::token_valid(&token) || sources.is_empty() || sources.len() > 100_000 {
                    return;
                }
                let mut state = self.state.lock().unwrap();
                if state.copies.contains_key(&token)
                    || state.queued.iter().any(|r| r.copy.as_ref().is_some_and(|(t, _)| t == &token))
                    || state.prompt.as_ref().is_some_and(|p| p.request.copy.as_ref().is_some_and(|(t, _)| t == &token))
                {
                    return;
                }
                state.queued.push_back(Request {
                    to_laptop,
                    operation,
                    copy: Some((token, sources)),
                    pane: 0,
                    side: Side::Left,
                    request: 0,
                    path,
                });
                drop(state);
                self.next();
            }
            ToClient::Ack(_) | ToClient::Data { .. } | ToClient::Installed | ToClient::Progress => {
                if let Some(worker) = self.state.lock().unwrap().copies.get(&token)
                    && worker.control.try_send(message).is_err()
                {
                    worker.cancelled.store(true, Ordering::Release);
                }
            }
            ToClient::Finish(result) => {
                let mut state = self.state.lock().unwrap();
                state.queued.retain(|r| r.copy.as_ref().is_none_or(|(t, _)| t != &token));
                if state.prompt.as_ref().is_some_and(|p| p.request.copy.as_ref().is_some_and(|(t, _)| t == &token)) {
                    state.prompt = None;
                }
                if let Some(worker) = state.copies.remove(&token) {
                    let result = if result.is_ok()
                        && (worker.destination.is_some() || worker.operation != Operation::Copy)
                        && !worker.completed
                    {
                        Err("Operation ended before local completion: check the affected paths before retrying".into())
                    } else {
                        result
                    };
                    if result.is_err() {
                        worker.cancelled.store(true, Ordering::Release);
                    }
                    let _ = worker.control.try_send(ToClient::Finish(result.clone()));
                    let server = self.destination.as_ref().map_or("server", |(_, label)| label.as_str());
                    let items = if worker.count == 1 { "item" } else { "items" };
                    let verb = match worker.operation {
                        Operation::Copy => "copied",
                        Operation::Move => "moved",
                        Operation::Delete => "deleted",
                    };
                    let text = match result {
                        Ok(()) => match worker.destination {
                            Some(path) => format!("{server} {verb} {} {items} to {}", worker.count, clean(&path)),
                            None => format!("{server} {verb} {} {items} from this laptop", worker.count),
                        },
                        Err(error) => format!("Laptop operation stopped: {}", clean(&error)),
                    };
                    state.notice = Some((Instant::now() + Duration::from_secs(4), text));
                    let this = Arc::downgrade(self);
                    let _ = crate::threads::named("laptop-notice").spawn(move || {
                        std::thread::sleep(Duration::from_secs(4));
                        if let Some(this) = this.upgrade() {
                            let mut state = this.state.lock().unwrap();
                            if state.notice.as_ref().is_some_and(|(until, _)| Instant::now() >= *until) {
                                state.notice = None;
                                drop(state);
                                this.send(&ClientMsg::Repaint);
                            }
                        }
                    });
                }
                drop(state);
                self.send(&ClientMsg::Repaint);
                self.draw();
                self.next();
            }
            _ => {}
        }
    }

    fn start_copy(self: &Arc<Self>, request: Request, scope: Scope, generation: u64) {
        let (token, sources) = request.copy.as_ref().unwrap().clone();
        let operation = request.operation;
        let (control, incoming) = crossbeam_channel::bounded(copy::WINDOW + 1);
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.state.lock().unwrap();
            if state.generation != generation {
                return;
            }
            state.copies.insert(
                token.clone(),
                CopyWorker {
                    operation,
                    control,
                    cancelled: cancelled.clone(),
                    count: sources.len(),
                    destination: (!request.path.is_empty()).then(|| request.path.clone()),
                    completed: false,
                },
            );
        }
        let this = self.clone();
        let retry = token.clone();
        let spawned = crate::threads::named("laptop-copy").spawn(move || {
            let result = (|| -> Result<()> {
                let mut reported = Instant::now();
                let mut check = || {
                    ensure!(
                        !cancelled.load(Ordering::Acquire),
                        "Laptop operation cancelled: check the source before retrying"
                    );
                    if reported.elapsed() >= Duration::from_millis(500) {
                        ensure!(
                            this.send_current(
                                generation,
                                &ClientMsg::LaptopCopy { token: token.clone(), message: FromClient::Progress }
                            ),
                            "Laptop connection lost: check the source before retrying"
                        );
                        reported = Instant::now();
                    }
                    Ok(())
                };
                if operation != Operation::Copy {
                    let (answer, reply) = crossbeam_channel::bounded(1);
                    {
                        let mut state = this.state.lock().unwrap();
                        ensure!(state.generation == generation, "Laptop connection lost");
                        ensure!(
                            !cancelled.load(Ordering::Acquire),
                            "Laptop operation cancelled: retry from the file manager"
                        );
                        state.prompt = Some(Prompt {
                            scroll: 0,
                            replacement: Some((sources.clone(), answer)),
                            request,
                            scope,
                            selected: 1,
                        });
                    }
                    this.draw();
                    ensure!(
                        reply.recv_timeout(copy::TIMEOUT).unwrap_or(false),
                        "Operation cancelled: nothing was removed. Select the items to try again"
                    );
                    check()?;
                }
                if operation == Operation::Delete {
                    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find the laptop home directory"))?;
                    let removal = copy::Removal::read(&sources, &home, scope, &mut check)
                        .context("Cannot inspect selected items: check them on the laptop before retrying")?;
                    removal.remove(check)
                        .context("Deletion stopped: check the selected items on the laptop before retrying")?;
                } else {
                    let removal = this.read_copy(&token, &sources, scope, generation, &cancelled, &incoming)?;
                    if operation == Operation::Copy {
                        return Ok(());
                    }
                    loop {
                        check()?;
                        match incoming.recv_timeout(copy::TIMEOUT) {
                            Ok(ToClient::Installed) => break,
                            Ok(ToClient::Progress) => {}
                            _ => bail!(
                                "Move stopped before installation was confirmed: source kept. Check both locations before retrying"
                            ),
                        }
                    }
                    removal.remove(check)
                        .context("Copy installed; source removal stopped. Check both locations before retrying")?;
                }
                {
                    let mut state = this.state.lock().unwrap();
                    ensure!(state.generation == generation, "Laptop connection lost");
                    if let Some(worker) = state.copies.get_mut(&token) {
                        worker.completed = true;
                    }
                }
                ensure!(
                    this.send_current(
                        generation,
                        &ClientMsg::LaptopCopy { token: token.clone(), message: FromClient::Finish(Ok(())) }
                    ),
                    "Laptop connection lost: check the source before retrying"
                );
                Ok(())
            })();
            this.copy_read_finished(&token, generation, result);
        });
        if let Err(error) = spawned {
            self.copy_read_finished(&retry, generation, Err(error.into()));
        }
    }

    fn start_write(self: &Arc<Self>, request: Request, scope: Scope, generation: u64) {
        let (token, sources) = request.copy.unwrap();
        let path = request.path;
        let operation = request.operation;
        let (control, incoming) = crossbeam_channel::bounded(copy::WINDOW + 1);
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.state.lock().unwrap();
            if state.generation != generation {
                return;
            }
            state.copies.insert(
                token.clone(),
                CopyWorker {
                    operation,
                    control,
                    cancelled: cancelled.clone(),
                    count: sources.len(),
                    destination: Some(path.clone()),
                    completed: false,
                },
            );
        }
        let this = self.clone();
        let retry = token.clone();
        let spawned = crate::threads::named("laptop-write").spawn(move || {
            let result = (|| -> Result<()> {
                let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find the laptop home directory"))?;
                let spec = copy::Spec {
                    operation,
                    to_laptop: true,
                    remote: None,
                    sources: sources.clone(),
                    destination: path.clone().into(),
                    allow: Vec::new(),
                };
                let mut stage = crate::laptop::stage::Stage::new(&spec, &home, scope)?;
                let replacements = stage.replacements();
                if !replacements.is_empty() {
                    let (answer, reply) = crossbeam_channel::bounded(1);
                    {
                        let mut state = this.state.lock().unwrap();
                        ensure!(
                            state.generation == generation && !cancelled.load(Ordering::Acquire),
                            "Laptop copy cancelled"
                        );
                        state.prompt = Some(Prompt {
                            scroll: 0,
                            replacement: Some((replacements, answer)),
                            request: Request {
                                to_laptop: true,
                                operation,
                                copy: Some((token.clone(), sources)),
                                pane: 0,
                                side: Side::Left,
                                request: 0,
                                path,
                            },
                            scope,
                            selected: 1,
                        });
                    }
                    this.draw();
                    ensure!(
                        reply.recv_timeout(copy::TIMEOUT).unwrap_or(false),
                        "Replacement cancelled: nothing was copied. Press c to ask again"
                    );
                }
                let check = || -> Result<()> {
                    ensure!(!cancelled.load(Ordering::Acquire), "Laptop copy cancelled: retry from the file manager");
                    Ok(())
                };
                check()?;
                ensure!(
                    this.send_current(
                        generation,
                        &ClientMsg::LaptopCopy { token: token.clone(), message: FromClient::Ack(0) }
                    ),
                    "Laptop connection lost"
                );
                let mut sequence = 0;
                loop {
                    check()?;
                    let message = incoming
                        .recv_timeout(copy::TIMEOUT)
                        .map_err(|_| anyhow::anyhow!("Laptop copy stalled: retry from the file manager"))?;
                    if matches!(message, ToClient::Progress) {
                        continue;
                    }
                    let ToClient::Data { sequence: received, packet } = message else {
                        bail!("Laptop copy cancelled: retry from the file manager")
                    };
                    ensure!(received == sequence, "out-of-order laptop copy data");
                    let done = stage.receive(packet)?;
                    ensure!(
                        this.send_current(
                            generation,
                            &ClientMsg::LaptopCopy { token: token.clone(), message: FromClient::Ack(sequence) }
                        ),
                        "Laptop connection lost"
                    );
                    sequence += 1;
                    if done {
                        break;
                    }
                }
                let mut reported = Instant::now();
                stage.install(|| {
                    check()?;
                    if reported.elapsed() >= Duration::from_millis(500) {
                        ensure!(
                            this.send_current(
                                generation,
                                &ClientMsg::LaptopCopy { token: token.clone(), message: FromClient::Progress }
                            ),
                            "Laptop connection lost"
                        );
                        reported = Instant::now();
                    }
                    Ok(())
                })
            })();
            if result.is_ok() {
                let mut state = this.state.lock().unwrap();
                if state.generation == generation
                    && let Some(worker) = state.copies.get_mut(&token)
                {
                    worker.completed = true;
                }
            }
            this.send_current(
                generation,
                &ClientMsg::LaptopCopy {
                    token: token.clone(),
                    message: FromClient::Finish(result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}"))),
                },
            );
            this.copy_read_finished(&token, generation, Ok(()));
        });
        if let Err(error) = spawned {
            self.copy_read_finished(&retry, generation, Err(error.into()));
        }
    }

    fn read_copy(
        &self,
        token: &str,
        sources: &[String],
        scope: Scope,
        generation: u64,
        cancelled: &AtomicBool,
        incoming: &crossbeam_channel::Receiver<ToClient>,
    ) -> Result<copy::Removal> {
        let check = || -> Result<()> {
            ensure!(!cancelled.load(Ordering::Acquire), "Laptop copy cancelled: retry from the file manager");
            Ok(())
        };
        let mut window = Window::new();
        let acknowledge = |window: &mut Window| -> Result<()> {
            check()?;
            match incoming.recv_timeout(copy::TIMEOUT) {
                Ok(ToClient::Ack(sequence)) => window.acknowledge(sequence),
                Ok(ToClient::Finish(Err(error))) => bail!("{error}"),
                _ => bail!("Laptop copy stalled or disconnected: reconnect and retry the copy"),
            }
        };
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find the laptop home directory"))?;
        copy::send(
            sources,
            &home,
            scope,
            |packet| {
                check()?;
                if window.full() {
                    acknowledge(&mut window)?;
                }
                let wait = matches!(packet, Packet::Begin { .. } | Packet::Done);
                let sequence = window.sent()?;
                ensure!(
                    self.send_current(
                        generation,
                        &ClientMsg::LaptopCopy { token: token.into(), message: FromClient::Data { sequence, packet } }
                    ),
                    "Laptop connection lost: reconnect and retry the copy"
                );
                if wait {
                    while !window.empty() {
                        acknowledge(&mut window)?;
                    }
                }
                Ok(())
            },
            check,
        )
    }

    fn copy_read_finished(self: &Arc<Self>, token: &str, generation: u64, result: Result<()>) {
        if let Err(error) = result {
            self.send_current(
                generation,
                &ClientMsg::LaptopCopy { token: token.into(), message: FromClient::Finish(Err(format!("{error:#}"))) },
            );
        }
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return;
        }
        state.busy = false;
        drop(state);
        self.next();
    }

    pub fn pending_keys(&self) -> bool {
        self.state.lock().unwrap().parser.has_pending()
    }

    pub fn input(self: &Arc<Self>, bytes: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap();
        // Observe every input chunk, including pastes that started before consent opened.
        let events = if bytes.is_empty() { state.parser.flush() } else { state.parser.feed(bytes) };
        if state.prompt.is_none() {
            return false;
        }
        let mut answer = None;
        for event in events {
            let Some(prompt) = state.prompt.as_mut() else {
                break;
            };
            let buttons = if prompt.replacement.is_some() { 2 } else { 3 };
            match event {
                InputEvent::Key(key) => match key.code {
                    KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
                        if prompt.replacement.is_some() =>
                    {
                        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                        let view = self.view(prompt);
                        let (max_scroll, page) = view.scroll_limits(Rect::new(0, 0, cols, rows.saturating_sub(1)));
                        let step = if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) { page } else { 1 };
                        prompt.scroll = if matches!(key.code, KeyCode::Up | KeyCode::PageUp) {
                            prompt.scroll.min(max_scroll).saturating_sub(step)
                        } else {
                            prompt.scroll.saturating_add(step).min(max_scroll)
                        };
                    }
                    KeyCode::Tab | KeyCode::Right => prompt.selected = (prompt.selected + 1) % buttons,
                    KeyCode::BackTab | KeyCode::Left => prompt.selected = (prompt.selected + buttons - 1) % buttons,
                    KeyCode::Enter => {
                        answer = Some(prompt.selected);
                        break;
                    }
                    KeyCode::Esc => {
                        answer = Some(buttons - 1);
                        break;
                    }
                    KeyCode::Char('c') if key.mods.ctrl => {
                        answer = Some(buttons - 1);
                        break;
                    }
                    _ => {}
                },
                InputEvent::Mouse(mouse)
                    if mouse.kind == crate::input::MouseKind::Press(crate::input::MouseButton::Left) =>
                {
                    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                    let (_, buttons) = self.view(prompt).render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
                    answer = buttons.iter().position(|r| r.contains((mouse.col, mouse.row).into()));
                    if answer.is_some() {
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(answer) = answer {
            let prompt = state.prompt.take().unwrap();
            if let Some((_, reply)) = prompt.replacement {
                let _ = reply.try_send(answer == 0);
                drop(state);
                self.send(&ClientMsg::Repaint);
                return true;
            }
            let retry = if prompt.request.copy.is_some() { "retry the copy" } else { "press r" };
            let mut error = None;
            if answer == 0 {
                state.allowed = true;
            }
            if answer == 1
                && let Some((ssh, label)) = &self.destination
            {
                error = paste::remember_laptop(&self.permissions, ssh, label).err().map(|e| {
                    format!("Cannot save laptop permission: {e}. Fix laptop-files.toml on the laptop and {retry}")
                });
            }
            state.busy = answer != 2 && error.is_none();
            let generation = state.generation;
            drop(state);
            self.send(&ClientMsg::Repaint);
            if answer == 2 {
                let host = self.identity.as_ref().map_or("laptop", |i| i.host.as_str());
                let message = if prompt.request.copy.is_some() {
                    format!("{host} refused: nothing was copied. Press c to ask again")
                } else {
                    format!("{host} refused: nothing was listed. Press r to ask again")
                };
                self.reply(prompt.request, Err(message));
                self.next();
            } else if let Some(error) = error {
                self.reply(prompt.request, Err(error));
                self.next();
            } else {
                self.start(prompt.request, prompt.scope, generation);
            }
        } else {
            drop(state);
            self.draw();
        }
        true
    }

    pub fn disconnect(&self) {
        let mut state = self.state.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        state.prompt = None;
        state.busy = false;
        state.queued.clear();
        state.ack = None;
        for (_, worker) in state.copies.drain() {
            worker.cancelled.store(true, Ordering::Release);
        }
        state.notice = None;
    }

    fn view(&self, prompt: &Prompt) -> View {
        let host = self.identity.as_ref().map_or("laptop", |i| i.host.as_str());
        let server = self.destination.as_ref().map_or("server", |(_, label)| label.as_str());
        if let Some((names, _)) = &prompt.replacement {
            let removing = !prompt.request.to_laptop && prompt.request.operation != Operation::Copy;
            let action = if !removing {
                "Replace"
            } else if prompt.request.operation == Operation::Move {
                "Move"
            } else {
                "Delete"
            };
            let message = if removing {
                if prompt.request.operation == Operation::Move {
                    format!(
                        "{server} asks to move these items to {} on {server}. Sources are removed only after installation and only if unchanged. Cancel keeps everything as it is.",
                        clean(&prompt.request.path)
                    )
                } else {
                    format!(
                        "{server} asks to delete these items for good, including folder contents. Cancel keeps everything as it is."
                    )
                }
            } else {
                format!(
                    "{server} asks to replace these items in {}. Folders are replaced in full. Cancel keeps everything as it is.",
                    clean(&prompt.request.path)
                )
            };
            return View {
                title: format!("{action} files on {}?", clean(host)),
                kind: NoticeKind::Warning,
                error_start: None,
                message,
                files: names.iter().map(|name| clean(name)).collect::<Vec<_>>().join("\n"),
                transfer: None,
                scroll: prompt.scroll,
                buttons: vec![action.into(), "Cancel".into()],
                selected: prompt.selected,
                keys: vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Cancel")],
            };
        }
        let scope = if prompt.scope == Scope::All {
            "all directories your user can access"
        } else if cfg!(target_os = "macos") {
            "your home directory, without its hidden top-level entries or ~/Library"
        } else {
            "your home directory, without its hidden top-level entries (~/.ssh, ~/.config)"
        };
        View {
            title: format!("Let {server} use the files of {}?", clean(host)),
            kind: NoticeKind::Notice,
            error_start: None,
            message: format!(
                "{server}'s file manager asks to list and copy files in {scope}.\nReplacing anything, deleting or moving files away from this laptop is asked in this box each time.\nAllow lasts until this kraal attach ends. Always is kept in ~/.config/kraal/laptop-files.toml."
            ),
            transfer: None,
            files: String::new(),
            scroll: 0,
            buttons: vec!["Allow".into(), format!("Always for {}", crate::text::truncate(server, 24)), "Cancel".into()],
            selected: prompt.selected,
            keys: vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Cancel")],
        }
    }

    fn draw(&self) {
        let _ = self.output(&[]);
    }

    pub fn output(&self, bytes: &[u8]) -> std::io::Result<bool> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        self.output_with(bytes, cols, rows, |bytes| super::terminal().write(bytes))
    }

    fn output_with(
        &self,
        bytes: &[u8],
        cols: u16,
        rows: u16,
        write: impl FnOnce(&[u8]) -> std::io::Result<bool>,
    ) -> std::io::Result<bool> {
        let state = self.state.lock().unwrap();
        let Some(prompt) = &state.prompt else {
            if let Some((until, message)) = &state.notice
                && Instant::now() < *until
            {
                let mut out = bytes.to_vec();
                let lines = crate::text::wrap(message, usize::from(cols).saturating_sub(2).max(1))
                    .into_iter()
                    .map(|line| (line, Some(NoticeKind::Notice)))
                    .collect::<Vec<_>>();
                out.extend(super::notice_lines(&lines, cols, rows));
                return write(&out);
            }
            return write(bytes);
        };
        let (buffer, _) = self.view(prompt).render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
        let mut out = b"\x1b[?2026h".to_vec();
        for part in bytes.split_inclusive(|b| *b == b'l') {
            out.extend_from_slice(part.strip_suffix(b"\x1b[?2026l").unwrap_or(part));
        }
        out.extend(crate::ansi::overlay(&buffer));
        write(&out)
    }
}

fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[derive(Clone, Default)]
    struct Wire(Arc<Mutex<Vec<u8>>>);
    impl Write for Wire {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn controller(permissions: PathBuf) -> (Arc<Controller>, Wire) {
        let peer = Wire::default();
        let controller = Arc::new(Controller {
            state: Mutex::new(State::default()),
            writer: Arc::new(Mutex::new(Some(Box::new(peer.clone())))),
            destination: Some((vec!["-p".into(), "2222".into(), "me@anvil".into()], "anvil".into())),
            identity: Some(Identity { id: "abc123".into(), host: "travel".into(), os: "linux".into() }),
            permissions,
        });
        (controller, peer)
    }

    fn result(peer: &mut Wire) -> Listing {
        let start = std::time::Instant::now();
        loop {
            let mut bytes = peer.0.lock().unwrap();
            let mut input = bytes.as_slice();
            while let Some((_, payload)) = protocol::read_frame(&mut input).unwrap() {
                if let ClientMsg::LaptopListed { result, .. } = serde_json::from_slice(&payload).unwrap() {
                    bytes.clear();
                    return result;
                }
            }
            drop(bytes);
            assert!(start.elapsed() < crate::testing::PATIENCE, "listing never answered");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn nothing(peer: &mut Wire) {
        assert!(peer.0.lock().unwrap().is_empty(), "nothing may be sent before consent");
    }

    #[test]
    fn consent_opened_during_a_paste_cannot_allow_or_remember_it() {
        let dir = TempDir::new("laptop-paste-consent");
        for answer in [b"\r".as_slice(), b"\t\r"] {
            let paste = paste::bracket(answer);
            for split in 1..paste.len() {
                let permissions = dir.0.join("laptop-files.toml");
                let (client, mut peer) = controller(permissions.clone());
                let target = super::super::Target::Remote {
                    ssh: vec!["box".into()],
                    label: "box".into(),
                    host: "box".into(),
                    session: "main".into(),
                    pane: None,
                    clipboard_native: false,
                };
                let attachments = super::super::attachments::Controller::for_target(
                    client.writer.clone(),
                    &target,
                    Some(client.clone()),
                );
                attachments.connected();
                attachments.input(&paste[..split]);
                peer.0.lock().unwrap().clear();
                client.request(1, Side::Left, 1, "~".into());
                attachments.input(&paste[split..]);
                attachments.input(b"");
                nothing(&mut peer);
                let state = client.state.lock().unwrap();
                assert_eq!(state.prompt.as_ref().unwrap().selected, 0, "split {split}");
                assert!(!state.allowed && !state.busy);
                drop(state);
                assert!(!permissions.exists());
                attachments.input(b"\x03");
                assert!(result(&mut peer).unwrap_err().contains("refused: nothing was listed"));
                attachments.input(b"typing after consent");
                let bytes = peer.0.lock().unwrap();
                let frame = protocol::read_frame(&mut bytes.as_slice()).unwrap().unwrap();
                assert_eq!(frame, (protocol::BYTES, b"typing after consent".to_vec()));
            }
        }
    }

    #[test]
    fn listing_requires_consent_always_survives_attach_and_removal_revokes_it() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::listing_requires_consent_always_survives_attach_and_removal_revokes_it",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-consent");
        let permissions = dir.0.join("laptop-files.toml");
        let home = dirs::home_dir().unwrap();
        std::fs::write(home.join("laptop-visible.txt"), "visible").unwrap();
        let (client, mut peer) = controller(permissions.clone());
        client.request(1, Side::Left, 1, "~".into());
        nothing(&mut peer);
        assert!(client.state.lock().unwrap().prompt.is_some());
        client.input(b"\x1b[200~\r\x1b[201~");
        nothing(&mut peer);
        client.input(b"\x1b");
        client.input(b"");
        assert!(result(&mut peer).unwrap_err().contains("refused: nothing was listed"));
        assert!(!permissions.exists());
        client.request(1, Side::Left, 2, "~".into());
        nothing(&mut peer);
        client.input(b"\t\r");
        assert!(result(&mut peer).unwrap().1.iter().any(|e| e.name == "laptop-visible.txt"));
        assert!(permissions.exists());
        assert!(!client.state.lock().unwrap().allowed);
        let (next, mut next_peer) = controller(permissions.clone());
        next.request(1, Side::Left, 3, "~".into());
        assert!(result(&mut next_peer).is_ok());
        assert!(next.state.lock().unwrap().prompt.is_none());
        std::fs::remove_file(&permissions).unwrap();
        next.request(1, Side::Left, 4, "~".into());
        nothing(&mut next_peer);
        assert!(next.state.lock().unwrap().prompt.is_some());
        next.input(b"\r");
        assert!(result(&mut next_peer).is_ok());
        assert!(!permissions.exists());
        next.disconnect();
        next.request(1, Side::Left, 5, "~".into());
        assert!(result(&mut next_peer).is_ok(), "Allow survives a reconnect");
        let (fresh, mut fresh_peer) = controller(permissions);
        fresh.request(1, Side::Left, 6, "~".into());
        nothing(&mut fresh_peer);
        assert!(fresh.state.lock().unwrap().prompt.is_some(), "Allow ends with attach");
        fresh.disconnect();
        assert!(fresh.state.lock().unwrap().prompt.is_none());
        assert!(!fresh.input(b"\r"));
        nothing(&mut fresh_peer);
    }

    #[test]
    fn legacy_listing_grant_requires_fresh_consent_before_copying() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::legacy_listing_grant_requires_fresh_consent_before_copying",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-upgrade-consent");
        let permissions =
            dir.write("laptop-files.toml", "[[server]]\nname = 'anvil'\nssh = ['-p', '2222', 'me@anvil']\n");
        let (client, mut peer) = controller(permissions.clone());
        let (ssh, _) = client.destination.as_ref().unwrap();
        assert!(paste::allowed(&permissions, ssh));
        assert!(!paste::laptop_allowed(&permissions, ssh));
        client.copy_message("abc".into(), ToClient::Start { sources: vec!["~/file".into()] });
        nothing(&mut peer);
        let state = client.state.lock().unwrap();
        let prompt = state.prompt.as_ref().unwrap();
        assert!(client.view(prompt).message.contains("list and copy files"));
        drop(state);
        client.input(b"\x03");
        assert!(matches!(copy_reply(&mut peer).1, FromClient::Finish(Err(_))));
        assert!(!paste::laptop_allowed(&permissions, ssh));
        // Remember the expanded permission through the local prompt, then attach again.
        client.request(1, Side::Left, 1, "~".into());
        nothing(&mut peer);
        client.input(b"\t\r");
        assert!(result(&mut peer).is_ok());
        assert!(paste::allowed(&permissions, ssh), "the old listing grant is retained");
        assert!(paste::laptop_allowed(&permissions, ssh));
        let (next, mut next_peer) = controller(permissions);
        next.request(1, Side::Left, 2, "~".into());
        assert!(result(&mut next_peer).is_ok());
        assert!(next.state.lock().unwrap().prompt.is_none());
    }

    #[test]
    fn scope_off_and_invalid_config_refuse_even_a_remembered_server() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::scope_off_and_invalid_config_refuse_even_a_remembered_server",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-off");
        let (client, mut peer) = controller(dir.0.join("laptop-files.toml"));
        let (ssh, name) = client.destination.as_ref().unwrap();
        paste::remember_laptop(&client.permissions, ssh, name).unwrap();
        let config = crate::paths::config_path();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "[laptop_files]\nscope = 'off'\n").unwrap();
        client.request(1, Side::Left, 1, "~".into());
        assert!(result(&mut peer).unwrap_err().contains("off"));
        std::fs::write(&config, "invalid [").unwrap();
        client.request(1, Side::Left, 2, "~".into());
        assert!(result(&mut peer).unwrap_err().contains("configuration"));
        assert!(client.state.lock().unwrap().prompt.is_none());
    }

    #[test]
    fn consent_names_come_from_the_client_and_describe_listing_and_copying() {
        let dir = TempDir::new("laptop-dialog");
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        let prompt = Prompt {
            scroll: 0,
            replacement: None,
            request: Request {
                to_laptop: false,
                operation: Operation::Copy,
                copy: None,
                pane: 1,
                side: Side::Left,
                request: 1,
                path: "server-controlled".into(),
            },
            scope: Scope::Home,
            selected: 0,
        };
        for (width, height) in [(80, 24), (124, 36), (20, 10), (1, 1)] {
            let view = client.view(&prompt);
            assert_eq!(view.title, "Let anvil use the files of travel?");
            assert!(!view.message.contains("server-controlled"));
            let (buf, _) = view.render(Rect::new(0, 0, width, height));
            if width >= 80 {
                let text = crate::testing::buffer_text(&buf);
                assert!(text.contains("Allow") && text.contains("Always for anvil") && text.contains("Cancel"));
                assert!(text.contains("Replacing anything"));
            }
        }
    }
    #[test]
    fn a_listing_finished_after_disconnect_cannot_answer_the_new_connection() {
        let dir = TempDir::new("laptop-stale");
        let (client, mut peer) = controller(dir.0.join("laptop-files.toml"));
        client.disconnect();
        client.state.lock().unwrap().busy = true;
        client.finish(
            0,
            Request {
                to_laptop: false,
                operation: Operation::Copy,
                copy: None,
                pane: 1,
                side: Side::Left,
                request: 1,
                path: "~".into(),
            },
            Ok(("~".into(), Vec::new())),
        );
        nothing(&mut peer);
        assert!(client.state.lock().unwrap().busy, "the current request is still running");
    }
    #[test]
    fn incoming_output_cannot_replace_the_local_question() {
        let dir = TempDir::new("laptop-frame");
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        client.state.lock().unwrap().prompt = Some(Prompt {
            scroll: 0,
            replacement: None,
            request: Request {
                to_laptop: false,
                operation: Operation::Copy,
                copy: None,
                pane: 1,
                side: Side::Left,
                request: 1,
                path: "~".into(),
            },
            scope: Scope::Home,
            selected: 0,
        });
        client
            .output_with(b"\x1b[?2026hSERVER FRAME\x1b[?2026l", 80, 24, |bytes| {
                let text = String::from_utf8_lossy(bytes);
                assert!(text.find("SERVER FRAME").unwrap() < text.find("Let anvil").unwrap());
                assert_eq!(text.matches("\x1b[?2026l").count(), 1);
                assert!(text.ends_with("\x1b[?2026l"));
                assert!(client.state.try_lock().is_err(), "input waits until the local question is drawn");
                Ok(true)
            })
            .unwrap();
    }

    #[test]
    fn always_that_cannot_be_saved_lists_nothing_and_can_be_retried() {
        let dir = TempDir::new("laptop-permission-failure");
        let (client, mut peer) = controller(dir.0.clone());
        client.request(1, Side::Left, 1, "~".into());
        nothing(&mut peer);
        client.input(b"\t\r");
        assert!(result(&mut peer).unwrap_err().contains("Cannot save laptop permission"));
        let state = client.state.lock().unwrap();
        assert!(!state.busy && !state.allowed && state.prompt.is_none());
        drop(state);
        client.request(1, Side::Left, 2, "~".into());
        nothing(&mut peer);
        assert!(client.state.lock().unwrap().prompt.is_some());
    }
    #[test]
    fn sending_a_listing_does_not_hold_local_input_or_disconnect() {
        struct Blocked {
            entered: crossbeam_channel::Sender<()>,
            resume: crossbeam_channel::Receiver<()>,
        }
        impl Write for Blocked {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.entered.send(()).unwrap();
                self.resume.recv_timeout(crate::testing::PATIENCE).unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = TempDir::new("laptop-slow-reply");
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        let (entered, receiving) = crossbeam_channel::bounded(1);
        let (release, resume) = crossbeam_channel::bounded(1);
        *client.writer.lock().unwrap() = Some(Box::new(Blocked { entered, resume }));
        let worker = client.clone();
        let thread = std::thread::spawn(move || {
            worker.finish(
                0,
                Request {
                    to_laptop: false,
                    operation: Operation::Copy,
                    copy: None,
                    pane: 1,
                    side: Side::Left,
                    request: 1,
                    path: "~".into(),
                },
                Ok(("~".into(), Vec::new())),
            )
        });
        receiving.recv_timeout(crate::testing::PATIENCE).unwrap();
        let available = client.state.try_lock().is_ok();
        if available {
            client.disconnect();
        }
        release.send(()).unwrap();
        thread.join().unwrap();
        assert!(available);
    }

    struct Peer(std::io::PipeReader);
    impl std::io::Read for Peer {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            use std::os::fd::AsRawFd;
            let mut fd = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd for the pipe this reader owns.
            assert!(
                unsafe { libc::poll(&mut fd, 1, crate::testing::PATIENCE.as_millis() as i32) } > 0,
                "the listing blocked control traffic"
            );
            self.0.read(bytes)
        }
    }

    fn pipe_peer(client: &Controller) -> Peer {
        let (peer, writer) = std::io::pipe().unwrap();
        *client.writer.lock().unwrap() =
            Some(Box::new(super::super::writer::TimedWriter::new(writer, Duration::from_secs(2), None).unwrap()));
        Peer(peer)
    }

    fn chunk(peer: &mut impl std::io::Read) -> (PaneId, Side, u64, Vec<u8>, bool) {
        let (kind, bytes) = protocol::read_frame(peer).unwrap().unwrap();
        assert_eq!(kind, protocol::JSON);
        if matches!(serde_json::from_slice::<ClientMsg>(&bytes).unwrap(), ClientMsg::LaptopCaseSensitive { .. }) {
            return chunk(peer);
        }
        let ClientMsg::LaptopListChunk { pane, side, request, data, done } = serde_json::from_slice(&bytes).unwrap()
        else {
            panic!("expected a listing chunk")
        };
        assert!(data.len() <= protocol::LAPTOP_CHUNK);
        (pane, side, request, data, done)
    }

    fn large_listing() -> Listing {
        Ok((
            "~".into(),
            (0..10000)
                .map(|i| crate::files::Entry {
                    name: format!("{i}-{}", "x".repeat(200)),
                    is_dir: false,
                    is_link: false,
                    size: 1,
                    mtime: 0,
                    exact: true,
                })
                .collect(),
        ))
    }

    #[test]
    fn keyboard_resize_and_detach_arrive_during_a_throttled_listing() {
        let dir = TempDir::new("laptop-throttled");
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        let mut peer = pipe_peer(&client);
        let expected = large_listing();
        let result = expected.clone();
        let worker = client.clone();
        let thread = std::thread::spawn(move || {
            worker.finish(
                0,
                Request {
                    to_laptop: false,
                    operation: Operation::Copy,
                    copy: None,
                    pane: 1,
                    side: Side::Left,
                    request: 1,
                    path: "~".into(),
                },
                result,
            )
        });
        let (_, _, _, mut listing, done) = chunk(&mut peer);
        assert!(!done);
        // Throttle the listing by withholding its acknowledgement. Control traffic must
        // reach the peer before it lets even the second chunk through.
        let sender = client.clone();
        let controls = std::thread::spawn(move || {
            assert!(!sender.input(b"typing"));
            let mut writer = sender.writer.lock().unwrap();
            let writer = writer.as_mut().unwrap();
            protocol::write_frame(writer, protocol::BYTES, b"typing").unwrap();
            protocol::write_json(writer, &ClientMsg::Resize { cols: 120, rows: 40 }).unwrap();
            protocol::write_frame(writer, protocol::BYTES, b"\x02d").unwrap();
        });
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"typing".to_vec()));
        let (_, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
        assert!(matches!(serde_json::from_slice(&bytes).unwrap(), ClientMsg::Resize { cols: 120, rows: 40 }));
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"\x02d".to_vec()));
        controls.join().unwrap();
        client.acknowledge(1, Side::Left, 1);
        loop {
            let (pane, side, request, data, done) = chunk(&mut peer);
            listing.extend(data);
            client.acknowledge(pane, side, request);
            if done {
                break;
            }
        }
        thread.join().unwrap();
        assert_eq!(listing, serde_json::to_vec(&expected).unwrap());
    }

    #[test]
    fn a_missing_listing_acknowledgement_aborts_and_keeps_the_connection_usable() {
        let dir = TempDir::new("laptop-stalled-ack");
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        let mut peer = pipe_peer(&client);
        let worker = client.clone();
        let thread = std::thread::spawn(move || {
            worker.finish(
                0,
                Request {
                    to_laptop: false,
                    operation: Operation::Copy,
                    copy: None,
                    pane: 1,
                    side: Side::Left,
                    request: 1,
                    path: "~".into(),
                },
                large_listing(),
            )
        });
        let (_, _, _, _, done) = chunk(&mut peer);
        assert!(!done);
        client.acknowledge(2, Side::Left, 1);
        client.acknowledge(1, Side::Right, 1);
        client.acknowledge(1, Side::Left, 0);
        let (_, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
        assert!(matches!(serde_json::from_slice(&bytes).unwrap(),
            ClientMsg::LaptopListed { result: Err(error), .. } if error.contains("stalled")));
        thread.join().unwrap();
        assert!(!client.state.lock().unwrap().busy);
        client.send(&ClientMsg::Detach);
        let (_, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
        assert!(matches!(serde_json::from_slice(&bytes).unwrap(), ClientMsg::Detach));
    }

    #[test]
    fn disconnect_during_a_chunked_listing_never_writes_to_the_next_connection() {
        let dir = TempDir::new("laptop-chunk-disconnect");
        let (client, mut next) = controller(dir.0.join("laptop-files.toml"));
        let mut peer = pipe_peer(&client);
        let worker = client.clone();
        let thread = std::thread::spawn(move || {
            worker.finish(
                0,
                Request {
                    to_laptop: false,
                    operation: Operation::Copy,
                    copy: None,
                    pane: 1,
                    side: Side::Left,
                    request: 1,
                    path: "~".into(),
                },
                large_listing(),
            )
        });
        assert!(!chunk(&mut peer).4);
        client.disconnect();
        *client.writer.lock().unwrap() = Some(Box::new(next.clone()));
        client.state.lock().unwrap().busy = true;
        thread.join().unwrap();
        nothing(&mut next);
        assert!(client.state.lock().unwrap().busy);
    }

    #[test]
    fn reconnecting_two_laptop_panels_queues_the_second_until_the_first_finishes() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::reconnecting_two_laptop_panels_queues_the_second_until_the_first_finishes",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-two-panels");
        let home = dirs::home_dir().unwrap();
        for i in 0..100 {
            std::fs::write(home.join(format!("entry-{i}")), "x").unwrap();
        }
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        let mut peer = pipe_peer(&client);
        client.state.lock().unwrap().allowed = true;
        client.disconnect();
        let mut panes = [1, 2].map(|_| {
            let location =
                crate::files::Location::Laptop { owner_id: "abc123".into(), host: "travel".into(), path: "~".into() };
            let local = crate::files::Location::Local(dir.0.clone());
            let mut pane = crate::files::FilesPane::new(local.clone(), local, Arc::new(|| {}));
            assert!(pane.set_laptop(Side::Left, location, 7));
            pane.forget_viewer(7);
            pane.reconnect_laptop("abc123", "travel", 8);
            pane
        });
        let first = panes[0].laptop_requests().pop().unwrap();
        client.request(1, first.1, first.2, first.3);
        let (p, side, request, data, done) = chunk(&mut peer);
        assert_eq!(p, 1);
        assert!(!done);
        panes[0].laptop_chunk(8, side, request, data, done);
        let second = panes[1].laptop_requests().pop().unwrap();
        client.request(2, second.1, second.2, second.3);
        assert_eq!(client.state.lock().unwrap().queued.len(), 1);
        client.acknowledge(p, side, request);
        let mut finished = Vec::new();
        while finished.len() < 2 {
            let (p, side, request, data, done) = chunk(&mut peer);
            panes[p as usize - 1].laptop_chunk(8, side, request, data, done);
            client.acknowledge(p, side, request);
            if done {
                finished.push(p);
            }
        }
        assert_eq!(finished, [1, 2]);
        for pane in &mut panes {
            pane.set_viewer(8);
            let area = Rect::new(0, 0, 120, 24);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            pane.render(area, &mut buffer);
            let text = crate::testing::buffer_text(&buffer);
            assert!(text.contains("entry-0"), "{text}");
            assert!(!text.contains("pending") && !text.contains("not attached"));
        }
    }

    #[test]
    fn queued_requests_recheck_permission_and_disconnect_discards_the_queue() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::queued_requests_recheck_permission_and_disconnect_discards_the_queue",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-queued-permission");
        let (client, mut peer) = controller(dir.0.join("laptop-files.toml"));
        let (ssh, name) = client.destination.as_ref().unwrap();
        paste::remember_laptop(&client.permissions, ssh, name).unwrap();
        client.state.lock().unwrap().busy = true;
        client.request(2, Side::Left, 2, "~".into());
        std::fs::remove_file(&client.permissions).unwrap();
        client.finish(
            0,
            Request {
                to_laptop: false,
                operation: Operation::Copy,
                copy: None,
                pane: 1,
                side: Side::Left,
                request: 1,
                path: "~".into(),
            },
            Ok(("~".into(), Vec::new())),
        );
        assert!(result(&mut peer).is_ok());
        assert_eq!(client.state.lock().unwrap().prompt.as_ref().unwrap().request.pane, 2);
        client.request(3, Side::Left, 3, "~".into());
        client.disconnect();
        let state = client.state.lock().unwrap();
        assert!(state.queued.is_empty() && state.prompt.is_none() && !state.busy);
        drop(state);
        client.finish(
            0,
            Request {
                to_laptop: false,
                operation: Operation::Copy,
                copy: None,
                pane: 3,
                side: Side::Left,
                request: 3,
                path: "~".into(),
            },
            Ok(("~".into(), Vec::new())),
        );
        nothing(&mut peer);
        client.state.lock().unwrap().busy = true;
        client.request(4, Side::Left, 4, "~".into());
        let config = crate::paths::config_path();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(config, "[laptop_files]\nscope = 'off'\n").unwrap();
        client.state.lock().unwrap().busy = false;
        client.next();
        assert!(result(&mut peer).unwrap_err().contains("off"));
    }
    fn copy_packet(peer: &mut impl std::io::Read) -> (String, FromClient) {
        loop {
            let (_, bytes) = protocol::read_frame(peer).unwrap().unwrap();
            if let ClientMsg::LaptopCopy { token, message } = serde_json::from_slice(&bytes).unwrap() {
                return (token, message);
            }
        }
    }

    fn copy_reply(peer: &mut Wire) -> (String, FromClient) {
        let bytes = std::mem::take(&mut *peer.0.lock().unwrap());
        copy_packet(&mut bytes.as_slice())
    }

    #[test]
    fn copy_permission_is_local_rechecked_and_cancel_sends_no_file_data() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::copy_permission_is_local_rechecked_and_cancel_sends_no_file_data",
        ) {
            return;
        }
        let dir = TempDir::new("copy-consent");
        let (client, mut peer) = controller(dir.0.join("laptop-files.toml"));
        client.copy_message("abc".into(), ToClient::Start { sources: vec!["~/.ssh/key".into()] });
        nothing(&mut peer);
        assert!(client.state.lock().unwrap().prompt.is_some());
        client.input(b"\x03");
        assert!(matches!(copy_reply(&mut peer), (token, FromClient::Finish(Err(_))) if token == "abc"));
        let (ssh, name) = client.destination.as_ref().unwrap();
        paste::remember_laptop(&client.permissions, ssh, name).unwrap();
        std::fs::remove_file(&client.permissions).unwrap();
        client.copy_message("def".into(), ToClient::Start { sources: vec!["~/file".into()] });
        assert!(client.state.lock().unwrap().prompt.is_some());
        client.copy_message("def".into(), ToClient::Finish(Err("cancelled".into())));
        assert!(client.state.lock().unwrap().prompt.is_none());
        let config = crate::paths::config_path();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(config, "[laptop_files]\nscope = 'off'\n").unwrap();
        client.state.lock().unwrap().allowed = true;
        client.copy_message("fed".into(), ToClient::Start { sources: vec!["~/file".into()] });
        assert!(
            matches!(copy_reply(&mut peer), (token, FromClient::Finish(Err(error))) if token == "fed" && error.contains("off"))
        );
    }

    #[test]
    fn copy_waits_for_total_ack_then_windows_data_without_blocking_input_and_disconnect() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::copy_waits_for_total_ack_then_windows_data_without_blocking_input_and_disconnect",
        ) {
            return;
        }
        let dir = TempDir::new("copy-window");
        std::fs::write(dirs::home_dir().unwrap().join("large"), vec![0; copy::CHUNK * 10]).unwrap();
        let (client, mut next) = controller(dir.0.join("laptop-files.toml"));
        let mut peer = pipe_peer(&client);
        client.state.lock().unwrap().allowed = true;
        client.copy_message("abc".into(), ToClient::Start { sources: vec!["~/large".into()] });
        assert!(
            matches!(copy_packet(&mut peer).1, FromClient::Data { sequence: 0, packet: Packet::Begin { bytes, .. } } if bytes == (copy::CHUNK * 10) as u64)
        );
        client.send(&ClientMsg::Resize { cols: 80, rows: 24 });
        let (_, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
        assert!(matches!(serde_json::from_slice(&bytes).unwrap(), ClientMsg::Resize { .. }));
        client.copy_message("abc".into(), ToClient::Ack(0));
        for expected in 1..=4 {
            assert!(matches!(copy_packet(&mut peer).1, FromClient::Data { sequence, .. } if sequence == expected));
        }
        assert!(!client.input(b"typing"));
        client.send(&ClientMsg::Detach);
        let (_, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
        assert!(
            matches!(serde_json::from_slice(&bytes).unwrap(), ClientMsg::Detach),
            "a fifth packet cannot pass the window"
        );
        client.disconnect();
        *client.writer.lock().unwrap() = Some(Box::new(next.clone()));
        client.state.lock().unwrap().busy = true;
        std::thread::sleep(Duration::from_millis(30));
        nothing(&mut next);
        assert!(client.state.lock().unwrap().busy);
        assert!(client.state.lock().unwrap().copies.is_empty());
    }

    fn wait_for_destructive_prompt(client: &Controller) {
        let started = Instant::now();
        while client.state.lock().unwrap().prompt.as_ref().is_none_or(|p| p.replacement.is_none()) {
            assert!(started.elapsed() < crate::testing::PATIENCE);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn delete_and_move_away_always_ask_locally_and_default_to_cancel() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::delete_and_move_away_always_ask_locally_and_default_to_cancel",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-destructive-consent");
        let home = dirs::home_dir().unwrap();
        let path = home.join("report");
        std::fs::write(&path, "original").unwrap();
        for operation in [Operation::Move, Operation::Delete] {
            let (client, mut peer) = controller(dir.0.join("laptop-files.toml"));
            let (ssh, label) = client.destination.as_ref().unwrap();
            paste::remember_laptop(&client.permissions, ssh, label).unwrap();
            let message = if operation == Operation::Delete {
                ToClient::Delete { sources: vec!["~/report".into()] }
            } else {
                ToClient::Move(copy::Spec {
                    operation,
                    to_laptop: false,
                    remote: None,
                    sources: vec!["~/report".into()],
                    destination: "/destination".into(),
                    allow: vec![],
                })
            };
            client.copy_message("abc".into(), message);
            wait_for_destructive_prompt(&client);
            nothing(&mut peer);
            {
                let state = client.state.lock().unwrap();
                let prompt = state.prompt.as_ref().unwrap();
                let view = client.view(prompt);
                assert_eq!(view.selected, 1);
                assert_eq!(
                    view.buttons,
                    if operation == Operation::Delete { vec!["Delete", "Cancel"] } else { vec!["Move", "Cancel"] }
                );
                assert!(view.title.contains("travel") && view.message.contains("anvil"));
                assert!(view.files.contains("~/report"));
                if operation == Operation::Move {
                    assert!(view.message.contains("/destination"));
                }
            }
            let pipe = pipe_peer(&client);
            client.input(&paste::bracket(b"\t\r"));
            assert!(client.state.lock().unwrap().prompt.is_some());
            client.input(b"\r");
            let mut peer = pipe;
            assert!(matches!(copy_packet(&mut peer).1, FromClient::Finish(Err(error)) if error.contains("cancelled")));
            assert_eq!(std::fs::read(&path).unwrap(), b"original");
            assert!(paste::laptop_allowed(&client.permissions, ssh));
        }
    }

    #[test]
    fn confirmed_delete_removes_items_and_asks_again_even_after_allow() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::confirmed_delete_removes_items_and_asks_again_even_after_allow",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-delete");
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        client.state.lock().unwrap().allowed = true;
        let mut peer = pipe_peer(&client);
        let path = dirs::home_dir().unwrap().join("report");
        for token in ["abc", "def"] {
            std::fs::write(&path, "original").unwrap();
            client.copy_message(token.into(), ToClient::Delete { sources: vec!["~/report".into()] });
            wait_for_destructive_prompt(&client);
            assert!(path.exists());
            client.input(b"\t\r");
            assert!(matches!(copy_packet(&mut peer).1, FromClient::Finish(Ok(()))));
            assert!(!path.exists());
            client.copy_message(token.into(), ToClient::Finish(Ok(())));
            assert_eq!(
                client.state.lock().unwrap().notice.as_ref().unwrap().1,
                "anvil deleted 1 item from this laptop"
            );
        }
    }

    #[test]
    fn move_away_waits_for_installation_and_keeps_changed_or_cancelled_sources() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::move_away_waits_for_installation_and_keeps_changed_or_cancelled_sources",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-move");
        let path = dirs::home_dir().unwrap().join("report");
        for outcome in ["success", "changed", "failed", "disconnect", "premature"] {
            std::fs::write(&path, "original").unwrap();
            let (client, _) = controller(dir.0.join("laptop-files.toml"));
            client.state.lock().unwrap().allowed = true;
            let mut peer = pipe_peer(&client);
            client.copy_message(
                "abc".into(),
                ToClient::Move(copy::Spec {
                    operation: Operation::Move,
                    to_laptop: false,
                    remote: None,
                    sources: vec!["~/report".into()],
                    destination: "~/work".into(),
                    allow: vec![],
                }),
            );
            wait_for_destructive_prompt(&client);
            {
                let state = client.state.lock().unwrap();
                let view = client.view(state.prompt.as_ref().unwrap());
                assert!(view.message.starts_with("anvil asks to move these items to ~/work on anvil."));
            }
            client.input(b"\t\r");
            loop {
                let FromClient::Data { sequence, packet } = copy_packet(&mut peer).1 else { panic!("expected data") };
                let done = matches!(packet, Packet::Done);
                client.copy_message("abc".into(), ToClient::Ack(sequence));
                if done {
                    break;
                }
            }
            assert_eq!(std::fs::read(&path).unwrap(), b"original");
            match outcome {
                "changed" => {
                    std::fs::write(&path, "changed contents").unwrap();
                }
                "failed" => {
                    client.copy_message("abc".into(), ToClient::Finish(Err("Installation failed: source kept".into())));
                    assert!(path.exists());
                    continue;
                }
                "disconnect" => {
                    client.disconnect();
                    assert!(path.exists());
                    continue;
                }
                "premature" => {
                    client.copy_message("abc".into(), ToClient::Finish(Ok(())));
                    assert!(path.exists());
                    assert!(!client.state.lock().unwrap().notice.as_ref().unwrap().1.contains(" moved "));
                    continue;
                }
                _ => {}
            }
            client.copy_message("abc".into(), ToClient::Installed);
            let FromClient::Finish(result) = copy_packet(&mut peer).1 else { panic!("expected completion") };
            assert_eq!(result.is_ok(), outcome == "success", "{result:?}");
            if outcome == "changed" {
                assert_eq!(std::fs::read(&path).unwrap(), b"changed contents");
                assert!(result.as_ref().unwrap_err().contains("Source changed"));
            } else {
                assert!(!path.exists());
            }
            client.copy_message("abc".into(), ToClient::Finish(result));
            if outcome == "success" {
                assert_eq!(client.state.lock().unwrap().notice.as_ref().unwrap().1, "anvil moved 1 item to ~/work");
            }
        }
    }

    #[test]
    fn operation_notices_use_local_names_last_four_seconds_and_do_not_consume_keys() {
        let dir = TempDir::new("copy-notice");
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        for (operation, destination, expected) in [
            (Operation::Copy, Some("~/Downloads"), "anvil copied 3 items to ~/Downloads"),
            (Operation::Move, Some("~/Downloads"), "anvil moved 3 items to ~/Downloads"),
            (Operation::Copy, None, "anvil copied 3 items from this laptop"),
            (Operation::Delete, None, "anvil deleted 3 items from this laptop"),
        ] {
            let (control, _incoming) = crossbeam_channel::bounded(1);
            client.state.lock().unwrap().copies.insert(
                "abc".into(),
                CopyWorker {
                    operation,
                    control,
                    cancelled: Arc::new(AtomicBool::new(false)),
                    count: 3,
                    destination: destination.map(str::to_owned),
                    completed: true,
                },
            );
            let before = Instant::now();
            client.copy_message("abc".into(), ToClient::Finish(Ok(())));
            let (until, text) = client.state.lock().unwrap().notice.clone().unwrap();
            assert_eq!(text, expected);
            assert!(until >= before + Duration::from_secs(4));
            assert!(until <= Instant::now() + Duration::from_secs(4));
            assert!(!client.input(b"typing"));
            client
                .output_with(b"", 80, 24, |bytes| {
                    assert!(String::from_utf8_lossy(bytes).contains(expected));
                    Ok(true)
                })
                .unwrap();
            client.state.lock().unwrap().notice.as_mut().unwrap().0 = Instant::now();
            client
                .output_with(b"", 80, 24, |bytes| {
                    assert!(bytes.is_empty());
                    Ok(true)
                })
                .unwrap();
        }
    }
    #[test]
    fn incoming_copy_replacement_always_asks_locally_and_cancel_writes_nothing() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::incoming_copy_replacement_always_asks_locally_and_cancel_writes_nothing",
        ) {
            return;
        }
        for operation in [Operation::Copy, Operation::Move] {
            let dir = TempDir::new("laptop-write-consent");
            let home = dirs::home_dir().unwrap();
            std::fs::write(home.join("report"), "old").unwrap();
            let (client, _) = controller(dir.0.join("laptop-files.toml"));
            let (ssh, label) = client.destination.as_ref().unwrap();
            paste::remember_laptop(&client.permissions, ssh, label).unwrap();
            let mut peer = pipe_peer(&client);
            client.copy_message(
                "abc".into(),
                ToClient::Write(copy::Spec {
                    operation,
                    to_laptop: true,
                    remote: None,
                    sources: vec!["/server/report".into()],
                    destination: "~".into(),
                    allow: vec!["report".into()],
                }),
            );
            let started = Instant::now();
            loop {
                if client.state.lock().unwrap().prompt.as_ref().is_some_and(|p| p.replacement.is_some()) {
                    break;
                }
                assert!(started.elapsed() < crate::testing::PATIENCE);
                std::thread::sleep(Duration::from_millis(5));
            }
            client
                .output_with(b"SERVER REPLACE APPROVED", 80, 24, |bytes| {
                    let text = String::from_utf8_lossy(bytes);
                    assert!(
                        text.contains("Replace files on travel?") && text.contains("anvil") && text.contains("report")
                    );
                    assert!(text.find("SERVER REPLACE").unwrap() < text.find("Replace files").unwrap());
                    Ok(true)
                })
                .unwrap();
            // Enter defaults to Cancel, and a pasted Replace shortcut cannot authorize it.
            client.input(&paste::bracket(b"\t\r"));
            assert!(client.state.lock().unwrap().prompt.is_some());
            client.input(b"\r");
            assert!(matches!(copy_packet(&mut peer).1, FromClient::Finish(Err(error)) if error.contains("cancelled")));
            assert_eq!(std::fs::read_to_string(home.join("report")).unwrap(), "old");
            assert!(
                !std::fs::read_dir(&home)
                    .unwrap()
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with(".kraal-stage-"))
            );
            assert!(paste::laptop_allowed(&client.permissions, ssh));
        }
    }

    #[test]
    fn incoming_copy_receives_only_after_preparation_and_finishes_without_server_approval() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::incoming_copy_receives_only_after_preparation_and_finishes_without_server_approval",
        ) {
            return;
        }
        for operation in [Operation::Copy, Operation::Move] {
            let path = dirs::home_dir().unwrap().join("report");
            let _ = std::fs::remove_file(&path);
            let dir = TempDir::new("laptop-write");
            let (client, _) = controller(dir.0.join("laptop-files.toml"));
            client.state.lock().unwrap().allowed = true;
            let mut peer = pipe_peer(&client);
            client.copy_message(
                "abc".into(),
                ToClient::Write(copy::Spec {
                    operation,
                    to_laptop: true,
                    remote: None,
                    sources: vec!["/server/report".into()],
                    destination: "~".into(),
                    allow: vec![],
                }),
            );
            assert!(matches!(copy_packet(&mut peer).1, FromClient::Ack(0)));
            client.copy_message("abc".into(), ToClient::Progress);
            let packets = [
                Packet::Begin { bytes: 3, entries: 1 },
                Packet::Entry(copy::Entry {
                    path: "report".into(),
                    kind: copy::Kind::File,
                    size: 3,
                    mode: 0o6755,
                    seconds: 1_600_000_000,
                    nanos: 0,
                }),
                Packet::Chunk("YWJj".into()),
                Packet::End,
                Packet::Done,
            ];
            for (sequence, packet) in packets.into_iter().enumerate() {
                client.copy_message("abc".into(), ToClient::Data { sequence: sequence as u64, packet });
                assert!(matches!(copy_packet(&mut peer).1, FromClient::Ack(received) if received == sequence as u64));
            }
            assert!(matches!(copy_packet(&mut peer).1, FromClient::Finish(Ok(()))));
            assert_eq!(std::fs::read(dirs::home_dir().unwrap().join("report")).unwrap(), b"abc");
            client.copy_message("abc".into(), ToClient::Finish(Ok(())));
            assert_eq!(
                client.state.lock().unwrap().notice.as_ref().unwrap().1,
                if operation == Operation::Copy { "anvil copied 1 item to ~" } else { "anvil moved 1 item to ~" }
            );
        }
    }
    #[test]
    fn incoming_partial_copy_disconnect_and_premature_success_remove_staging() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::incoming_partial_copy_disconnect_and_premature_success_remove_staging",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-write-interrupted");
        let home = dirs::home_dir().unwrap();
        for disconnect in [true, false] {
            let (client, _) = controller(dir.0.join("laptop-files.toml"));
            client.state.lock().unwrap().allowed = true;
            let mut peer = pipe_peer(&client);
            client.copy_message(
                "abc".into(),
                ToClient::Write(copy::Spec {
                    operation: crate::laptop::copy::Operation::Copy,
                    to_laptop: true,
                    remote: None,
                    sources: vec!["/server/report".into()],
                    destination: "~".into(),
                    allow: vec![],
                }),
            );
            assert!(matches!(copy_packet(&mut peer).1, FromClient::Ack(0)));
            client.copy_message(
                "abc".into(),
                ToClient::Data { sequence: 0, packet: Packet::Begin { bytes: 3, entries: 1 } },
            );
            assert!(matches!(copy_packet(&mut peer).1, FromClient::Ack(0)));
            client.copy_message(
                "abc".into(),
                ToClient::Data {
                    sequence: 1,
                    packet: Packet::Entry(copy::Entry {
                        path: "report".into(),
                        kind: copy::Kind::File,
                        size: 3,
                        mode: 0o644,
                        seconds: 0,
                        nanos: 0,
                    }),
                },
            );
            assert!(matches!(copy_packet(&mut peer).1, FromClient::Ack(1)));
            let staged = std::fs::read_dir(&home)
                .unwrap()
                .flatten()
                .find(|e| e.file_name().to_string_lossy().starts_with(".kraal-stage-"))
                .unwrap()
                .path();
            assert!(staged.join("payload/report").exists());
            if disconnect {
                client.disconnect();
            } else {
                client.copy_message("abc".into(), ToClient::Finish(Ok(())));
                assert!(client.state.lock().unwrap().notice.as_ref().unwrap().1.contains("before local completion"));
            }
            let started = Instant::now();
            while staged.exists() {
                assert!(started.elapsed() < crate::testing::PATIENCE, "cancelled stage was not removed");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!home.join("report").exists());
        }
    }

    #[test]
    fn a_replacement_requires_local_allow_and_long_lists_can_be_scrolled() {
        let dir = TempDir::new("laptop-replace-keys");
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        for cancel in [false, true] {
            let (answer, reply) = crossbeam_channel::bounded(1);
            client.state.lock().unwrap().prompt = Some(Prompt {
                scroll: 0,
                replacement: Some(((0..100).map(|n| format!("report-{n:03}")).collect(), answer)),
                request: Request {
                    to_laptop: false,
                    operation: Operation::Copy,
                    copy: Some(("abc".into(), vec![])),
                    pane: 0,
                    side: Side::Left,
                    request: 0,
                    path: "~/Downloads".into(),
                },
                scope: Scope::Home,
                selected: 1,
            });
            {
                let state = client.state.lock().unwrap();
                let view = client.view(state.prompt.as_ref().unwrap());
                assert!(view.transfer.is_none());
                assert!(view.title.starts_with("Replace files on "));
            }
            client.input(b"\x1b[6~");
            assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().scroll > 0);
            client.input(b"\x1b[5~");
            assert_eq!(client.state.lock().unwrap().prompt.as_ref().unwrap().scroll, 0);
            client.input(if cancel { b"\x1b" } else { b"\t\r" });
            client.input(b"");
            assert_eq!(reply.recv_timeout(crate::testing::PATIENCE).unwrap(), !cancel);
            assert!(client.state.lock().unwrap().prompt.is_none());
        }
    }

    #[test]
    fn a_remembered_read_grant_still_asks_before_writing_to_the_laptop() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::a_remembered_read_grant_still_asks_before_writing_to_the_laptop",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-old-read-grant");
        let path = dir.0.join("laptop-files.toml");
        std::fs::write(&path, "[[laptop_copy]]\nname = 'anvil'\nssh = ['-p', '2222', 'me@anvil']\n").unwrap();
        let (client, mut peer) = controller(path);
        client.copy_message(
            "abc".into(),
            ToClient::Write(copy::Spec {
                operation: crate::laptop::copy::Operation::Copy,
                to_laptop: true,
                remote: None,
                sources: vec!["/server/report".into()],
                destination: "~".into(),
                allow: vec![],
            }),
        );
        nothing(&mut peer);
        assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().replacement.is_none());
        client.input(b"\x03");
        assert!(matches!(copy_reply(&mut peer).1, FromClient::Finish(Err(_))));
        assert!(!dirs::home_dir().unwrap().join("report").exists());
    }
}
