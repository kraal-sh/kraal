use std::sync::atomic::Ordering;
use std::sync::{Arc, MutexGuard};
use std::time::Duration;

use super::{Controller, Prompt, Request, State};
use crate::client::paste;
use crate::files::Side;
use crate::laptop::copy::{FromClient, Operation};
use crate::laptop::{Listing, Scope};
use crate::layout::PaneId;
use crate::protocol::{self, ClientMsg};

impl Controller {
    pub(super) fn send(&self, message: &ClientMsg) {
        if let Ok(mut writer) = self.writer.lock()
            && let Some(writer) = writer.as_mut()
        {
            let _ = protocol::write_json(writer, message);
        }
    }

    pub(super) fn reply(&self, r: Request, result: Listing) {
        if let Some((token, _)) = r.copy {
            self.send(&ClientMsg::LaptopCopy { token, message: FromClient::Finish(result.map(|_| ())) });
        } else {
            self.send(&ClientMsg::LaptopListed { pane: r.pane, side: r.side, request: r.request, result });
        }
    }

    pub fn request(self: &Arc<Self>, pane: PaneId, side: Side, request: u64, path: String) {
        let request = Request { to_laptop: false, operation: Operation::Copy, copy: None, pane, side, request, path };
        let mut state = self.state.lock().unwrap();
        // The server ignores the answer to a panel's older listing.
        state.queued.retain(|r| r.copy.is_some() || (r.pane, r.side) != (pane, side));
        self.enqueue(state, request);
    }

    /// Queues `request`, or refuses it at once when the queue is full.
    pub(super) fn enqueue(self: &Arc<Self>, mut state: MutexGuard<'_, State>, request: Request) {
        let size = |r: &Request| {
            r.path.len() + r.copy.as_ref().map_or(0, |(_, sources)| sources.iter().map(String::len).sum())
        };
        if state.queued.len() >= super::QUEUED
            || state.queued.iter().map(size).sum::<usize>() + size(&request) > super::QUEUED_BYTES
        {
            drop(state);
            let retry = if request.copy.is_some() { "retry the copy" } else { "press r" };
            self.reply(request, Err(format!("Too many laptop requests are waiting: {retry} later")));
            return;
        }
        state.queued.push_back(request);
        drop(state);
        self.next();
    }

    pub(super) fn next(self: &Arc<Self>) {
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
                // Cancel is selected: the server can open this question while Enter is typed for a pane.
                state.prompt = Some(Prompt { scroll: 0, replacement: None, request, scope, selected: 2 });
                drop(state);
                self.draw();
            }
            return;
        }
    }

    pub(super) fn start(self: &Arc<Self>, request: Request, scope: Scope, generation: u64) {
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
            let result = crate::paths::home_dir()
                .ok_or_else(|| "Cannot find the laptop home directory".to_string())
                .and_then(|home| {
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

    pub(super) fn finish(self: &Arc<Self>, generation: u64, request: Request, result: Listing) {
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

    pub(super) fn send_current(&self, generation: u64, message: &ClientMsg) -> bool {
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
}
