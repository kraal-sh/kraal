use std::collections::HashMap;

use crossbeam_channel::{Receiver, Sender};

use super::requests::ReplyHandle;
use super::{Event, EventSender, Server};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub(super) u32);

type Continuation = Box<dyn FnOnce(&mut Server, Result<(), String>)>;

#[derive(Default)]
pub(super) struct Jobs {
    pending: HashMap<JobId, Continuation>,
}

pub(super) struct Delivery<T> {
    id: JobId,
    result: Sender<T>,
    events: EventSender,
}

impl Jobs {
    pub(super) fn register<T: Send + 'static>(
        &mut self,
        id: JobId,
        events: EventSender,
        then: impl FnOnce(&mut Server, T) + 'static,
    ) -> Delivery<T> {
        let (delivery, rx) = Delivery::channel(id, events);
        self.insert(id, rx, then);
        delivery
    }

    pub(super) fn insert<T: Send + 'static>(
        &mut self,
        id: JobId,
        rx: Receiver<T>,
        then: impl FnOnce(&mut Server, T) + 'static,
    ) {
        self.pending.insert(
            id,
            Box::new(move |server, result| {
                if result.is_ok()
                    && let Ok(value) = rx.try_recv()
                {
                    then(server, value);
                }
            }),
        );
    }

    pub(super) fn take(&mut self, id: JobId) -> Option<Continuation> {
        self.pending.remove(&id)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.pending.len()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

impl<T> Delivery<T> {
    pub(super) fn channel(id: JobId, events: EventSender) -> (Self, Receiver<T>) {
        let (result, rx) = crossbeam_channel::bounded(1);
        (Self { id, result, events }, rx)
    }

    pub(super) fn complete(self, result: std::thread::Result<T>) {
        let result = result.map(|value| {
            // The value must be ready before the event wakes the server.
            let _ = self.result.send(value);
        });
        let _ = self.events.send(Event::Job(self.id, result));
    }
}

impl<T: Send + 'static> Delivery<T> {
    fn start(self, work: impl FnOnce() -> T + Send + 'static) -> std::io::Result<std::thread::JoinHandle<()>> {
        #[cfg(test)]
        if FAIL_START.replace(false) {
            return Err(std::io::Error::other("thread start failed"));
        }
        crate::threads::named("job").spawn(move || {
            self.complete(std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)));
        })
    }
}

impl Server {
    /// Run `work` on a helper thread (slow git, commands), then `then` with its result on the
    /// server thread, so nothing slow ever blocks the event loop. `false` when no thread could
    /// be started: `then` never runs, nor does it when `work` panics (a warning says so).
    pub fn background<T: Send + 'static>(
        &mut self,
        work: impl FnOnce() -> T + Send + 'static,
        then: impl FnOnce(&mut Server, T) + 'static,
    ) -> bool {
        let id = JobId(self.alloc_id());
        let delivery = self.jobs.register(id, self.tx.clone(), then);
        if let Err(e) = delivery.start(work) {
            self.jobs.take(id);
            self.warnings.push(format!("cannot start a background job: {e}"));
            return false;
        }
        true
    }

    /// Keep the reply with the caller if the worker cannot start; answer it if the worker panics.
    pub(super) fn background_owned<T: Send + 'static, S: Into<Option<ReplyHandle>> + 'static>(
        &mut self,
        state: S,
        work: impl FnOnce() -> T + Send + 'static,
        then: impl FnOnce(&mut Server, S, T) + 'static,
    ) -> Result<(), S> {
        let id = JobId(self.alloc_id());
        let (delivery, rx) = Delivery::channel(id, self.tx.clone());
        if let Err(e) = delivery.start(work) {
            self.warnings.push(format!("cannot start a background job: {e}"));
            return Err(state);
        }
        self.jobs.pending.insert(
            id,
            Box::new(move |server, result| match result {
                Ok(()) => {
                    if let Ok(value) = rx.try_recv() {
                        then(server, state, value);
                    }
                }
                Err(why) => {
                    if let Some(reply) = state.into() {
                        reply.complete(server, Err(why));
                    }
                }
            }),
        );
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_START: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::server::{queue, tests::server};
    use crate::testing::PATIENCE;

    #[test]
    fn owned_replies_return_on_start_failure_and_transfer_on_success() {
        use crate::protocol::Response;
        use crate::server::requests::{
            ReplyHandle,
            tests::{finish_job, response},
        };
        for fail in [false, true] {
            let (tx, rx) = queue::channel(32);
            let mut server = server(tx);
            let (id, _peer) = crate::server::tests::client(&mut server);
            let receive = server.clients.get_mut(&id).unwrap().out.capture();
            FAIL_START.set(fail);
            let started = server.background_owned(
                ReplyHandle::new(id),
                || "ready",
                |server, reply, value| {
                    reply.complete(server, Ok(Response::Text(value.into())));
                },
            );
            assert!(receive.is_empty());
            if fail {
                let reply = started.err().unwrap();
                assert!(server.jobs.is_empty());
                assert_eq!(server.warnings, ["cannot start a background job: thread start failed"]);
                reply.complete(&mut server, Err("could not start".into()));
                assert_eq!(response(&receive).unwrap_err(), "could not start");
            } else {
                assert!(started.is_ok());
                finish_job(&mut server, &rx);
                assert!(matches!(response(&receive), Ok(Response::Text(text)) if text == "ready"));
            }
            assert!(server.jobs.is_empty());
            assert!(!server.clients.contains_key(&id));
            assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
        }
    }

    #[test]
    fn a_panicking_owned_worker_answers_and_releases_its_request() {
        use crate::server::requests::tests::{finish_job, response};
        for disconnected in [false, true] {
            for kind in 0..3 {
                let (tx, rx) = queue::channel(32);
                let mut server = server(tx);
                let (id, _peer) = crate::server::tests::client(&mut server);
                let receive = server.clients.get_mut(&id).unwrap().out.capture();
                assert!(
                    server
                        .background_owned(
                            ReplyHandle::new(id),
                            move || match kind {
                                0 => panic!("worker failed"),
                                1 => std::panic::panic_any(String::from("owned failure")),
                                _ => std::panic::panic_any(7),
                            },
                            |_, _, ()| panic!("not called"),
                        )
                        .is_ok()
                );
                if disconnected {
                    server.drop_client(id);
                }
                finish_job(&mut server, &rx);
                assert!(server.jobs.is_empty());
                let text = ["worker failed", "owned failure", "it panicked"][kind];
                let error = format!("a background job failed: {text}");
                assert_eq!(server.warnings, std::slice::from_ref(&error));
                if !disconnected {
                    assert!(!receive.is_empty());
                    assert_eq!(response(&receive).unwrap_err(), error);
                }
                assert!(!server.clients.contains_key(&id));
                assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
            }
        }
    }

    #[test]
    fn a_panicking_worker_answers_an_optional_reply() {
        use crate::server::requests::tests::{finish_job, response};
        let (tx, rx) = queue::channel(32);
        let mut server = server(tx);
        let (id, _peer) = crate::server::tests::client(&mut server);
        let receive = server.clients.get_mut(&id).unwrap().out.capture();
        assert!(
            server
                .background_owned(Some(ReplyHandle::new(id)), || panic!("failed"), |_, _, ()| panic!("not called"))
                .is_ok()
        );
        finish_job(&mut server, &rx);
        assert!(server.jobs.is_empty());
        assert_eq!(server.warnings, ["a background job failed: failed"]);
        assert_eq!(response(&receive).unwrap_err(), "a background job failed: failed");
        assert!(!server.clients.contains_key(&id));
        assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));

        assert!(server.background_owned(None, || panic!("no requester"), |_, _, ()| panic!("not called")).is_ok());
        finish_job(&mut server, &rx);
        assert!(server.jobs.is_empty());
        assert!(server.clients.is_empty());
        assert_eq!(server.warnings, ["a background job failed: failed", "a background job failed: no requester"]);
    }

    #[test]
    fn typed_results_follow_completion_order_on_the_server_thread() {
        let (tx, rx) = queue::channel(2);
        let mut server = server(tx);
        let owner = std::thread::current().id();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let first = JobId(server.alloc_id());
        let record = seen.clone();
        let text = server.jobs.register(first, server.tx.clone(), move |server, value: String| {
            assert_eq!(std::thread::current().id(), owner);
            assert!(server.jobs.is_empty(), "removed before the continuation runs");
            record.borrow_mut().push(value);
        });
        let second = JobId(server.alloc_id());
        let record = seen.clone();
        let number = server.jobs.register(second, server.tx.clone(), move |server, value: usize| {
            assert_eq!(std::thread::current().id(), owner);
            assert_eq!(server.jobs.len(), 1);
            record.borrow_mut().push(value.to_string());
        });
        assert_eq!((first, second, server.alloc_id()), (JobId(1), JobId(2), 3));
        std::thread::spawn(move || {
            number.complete(Ok(42));
            text.complete(Ok("ready".into()));
        })
        .join()
        .unwrap();
        assert!(seen.borrow().is_empty());
        for id in [second, first] {
            let event = rx.recv_timeout(PATIENCE).unwrap();
            assert!(matches!(event, Event::Job(got, Ok(())) if got == id));
            server.handle(event);
        }
        server.handle(Event::Job(first, Ok(())));
        assert_eq!(*seen.borrow(), ["42", "ready"]);
        assert!(server.warnings.is_empty());
    }

    #[test]
    fn background_delivers_once_and_can_start_another_job() {
        let (tx, rx) = queue::channel(1);
        let mut server = server(tx);
        assert!(server.background(
            || vec![3, 5],
            |server, values| {
                assert_eq!(values, [3, 5]);
                assert!(server.jobs.is_empty());
                assert!(server.background(|| "next".to_owned(), |server, value| server.warnings.push(value)));
            }
        ));
        assert_eq!(server.alloc_id(), 2);
        server.handle(rx.recv_timeout(PATIENCE).unwrap());
        server.handle(rx.recv_timeout(PATIENCE).unwrap());
        assert!(server.jobs.is_empty());
        assert_eq!(server.warnings, ["next"]);
    }

    #[test]
    fn a_worker_that_cannot_start_removes_the_job_without_running_either_closure() {
        let (tx, rx) = queue::channel(1);
        let mut server = server(tx);
        let held = Rc::new(());
        let continuation = held.clone();
        FAIL_START.set(true);
        assert!(!server.background(
            || panic!("work must not run"),
            move |_, ()| {
                drop(continuation);
                panic!("continuation must not run");
            }
        ));
        assert!(server.jobs.is_empty());
        assert_eq!(Rc::strong_count(&held), 1);
        assert!(rx.is_idle());
        assert_eq!(server.alloc_id(), 2, "a failed start still consumes its ID");
        assert_eq!(server.warnings, ["cannot start a background job: thread start failed"]);
    }

    #[test]
    fn panics_remove_jobs_and_keep_the_existing_warning_even_after_removal() {
        for removed in [false, true] {
            for kind in 0..3 {
                let (tx, rx) = queue::channel(1);
                let mut server = server(tx);
                let id = JobId(server.next_id);
                assert!(server.background(
                    move || match kind {
                        0 => panic!("worker failed"),
                        1 => std::panic::panic_any(String::from("owned failure")),
                        _ => std::panic::panic_any(7),
                    },
                    |_, ()| panic!("continuation must not run")
                ));
                if removed {
                    server.jobs.take(id);
                }
                server.handle(rx.recv_timeout(PATIENCE).unwrap());
                assert!(server.jobs.is_empty());
                let text = ["worker failed", "owned failure", "it panicked"][kind];
                assert_eq!(server.warnings, [format!("a background job failed: {text}")]);
            }
        }
    }

    #[test]
    fn a_removed_job_discards_its_result() {
        let (tx, rx) = queue::channel(1);
        let mut server = server(tx);
        let id = JobId(server.alloc_id());
        let delivery = server.jobs.register(id, server.tx.clone(), |_, _: String| panic!("removed job ran"));
        server.jobs.take(id);
        std::thread::spawn(move || delivery.complete(Ok("late".into()))).join().unwrap();
        server.handle(rx.recv_timeout(PATIENCE).unwrap());
        assert!(server.jobs.is_empty());
        assert!(server.warnings.is_empty());
    }

    #[test]
    fn a_result_for_a_client_that_left_still_completes_the_job() {
        let (tx, rx) = queue::channel(1);
        let mut server = server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        assert!(server.background(
            || String::from("finished"),
            move |server, text| server.toast(client, crate::theme::NoticeKind::Notice, text)
        ));
        server.clients.remove(&client);
        // Removing the client also ends its reader, whose ClientGone may arrive first.
        while !server.jobs.is_empty() {
            server.handle(rx.recv_timeout(PATIENCE).unwrap());
        }
        assert!(server.clients.is_empty());
        assert!(server.warnings.is_empty());
    }

    #[test]
    fn shutdown_drops_continuations_and_releases_workers_with_a_full_queue() {
        let (tx, rx) = queue::channel(1);
        let mut server = server(tx);
        assert!(server.tx.wake());
        let held = Rc::new(());
        let continuation = held.clone();
        let id = JobId(server.alloc_id());
        let delivery = server.jobs.register(id, server.tx.clone(), move |_, ()| {
            drop(continuation);
            panic!("continuation must not run");
        });
        let (finished, done) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            delivery.complete(Ok(()));
            finished.send(()).unwrap();
        });
        drop(server);
        assert_eq!(Rc::strong_count(&held), 1);
        drop(rx);
        done.recv_timeout(PATIENCE).unwrap();
        worker.join().unwrap();
    }
}
