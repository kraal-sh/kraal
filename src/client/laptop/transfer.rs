use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use super::{Controller, CopyWorker, Prompt, Request, clean};
use crate::files::Side;
use crate::laptop::copy::{FromClient, Operation, Packet, ToClient, Window};
use crate::laptop::{Scope, copy};
use crate::protocol::ClientMsg;

impl Controller {
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
                let state = self.state.lock().unwrap();
                if state.copies.contains_key(&token)
                    || state.queued.iter().any(|r| r.copy.as_ref().is_some_and(|(t, _)| t == &token))
                    || state.prompt.as_ref().is_some_and(|p| p.request.copy.as_ref().is_some_and(|(t, _)| t == &token))
                {
                    return;
                }
                let request = Request {
                    to_laptop,
                    operation,
                    copy: Some((token, sources)),
                    pane: 0,
                    side: Side::Left,
                    request: 0,
                    path,
                };
                self.enqueue(state, request);
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

    pub(super) fn start_copy(self: &Arc<Self>, request: Request, scope: Scope, generation: u64) {
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
                    let home = crate::paths::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find the laptop home directory"))?;
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

    pub(super) fn start_write(self: &Arc<Self>, request: Request, scope: Scope, generation: u64) {
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
                let home =
                    crate::paths::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find the laptop home directory"))?;
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
        let home = crate::paths::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find the laptop home directory"))?;
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
}
