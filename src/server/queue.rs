//! The server's event queue and the notifications whose state survives a full queue.

use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::{self, ThreadId};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};

use super::Event;
use crate::layout::PaneId;

const PANES: u8 = 1;
const WAKE: u8 = 2;

struct Domain {
    owner: ThreadId,
    dirty: AtomicU8,
}

#[derive(Clone)]
pub struct EventSender {
    events: Sender<Event>,
    token: Sender<()>,
    domain: Arc<Domain>,
}

pub struct EventReceiver {
    events: Receiver<Event>,
    token: Receiver<()>,
    domain: Arc<Domain>,
    // A receiver stays on the thread recorded at construction, even when moved into a server.
    _owner: PhantomData<Rc<()>>,
}

pub enum SendError {
    OwnThread(Event),
    Disconnected(Event),
}

impl std::fmt::Debug for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, event) = match self {
            Self::OwnThread(event) => ("OwnThread", event),
            Self::Disconnected(event) => ("Disconnected", event),
        };
        f.debug_tuple(kind).field(&std::mem::discriminant(event)).finish()
    }
}

pub enum Wait {
    Event(Event),
    Overflow,
    Timeout,
    Disconnected,
}

#[derive(Default)]
pub struct Notifications {
    pub panes: bool,
    pub wake: bool,
}

/// Bind a queue to this thread before any restore or handler can start producers.
pub fn channel(capacity: usize) -> (EventSender, EventReceiver) {
    let (events, rx) = crossbeam_channel::bounded(capacity);
    let (token, tokens) = crossbeam_channel::bounded(1);
    let domain = Arc::new(Domain { owner: thread::current().id(), dirty: AtomicU8::new(0) });
    (
        EventSender { events, token, domain: domain.clone() },
        EventReceiver { events: rx, token: tokens, domain, _owner: PhantomData },
    )
}

impl EventSender {
    pub fn send(&self, event: Event) -> Result<(), SendError> {
        if thread::current().id() == self.domain.owner {
            return Err(SendError::OwnThread(event));
        }
        self.events.send(event).map_err(|e| SendError::Disconnected(e.0))
    }

    pub fn pane_ready(&self, id: PaneId) -> bool {
        self.notify(Event::PaneReady(id), PANES)
    }

    pub fn wake(&self) -> bool {
        self.notify(Event::Wake, WAKE)
    }

    fn notify(&self, event: Event, bit: u8) -> bool {
        match self.events.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Disconnected(_)) => false,
            Err(TrySendError::Full(_)) => {
                self.mark(bit);
                // Even an already-set bit needs a token: the loop may have taken the old
                // token and exchanged the bits while this publisher was paused.
                self.signal()
            }
        }
    }

    fn mark(&self, bit: u8) {
        self.domain.dirty.fetch_or(bit, Ordering::SeqCst);
    }

    fn signal(&self) -> bool {
        !matches!(self.token.try_send(()), Err(TrySendError::Disconnected(())))
    }
}

impl EventReceiver {
    pub fn assert_sender(&self, sender: &EventSender) {
        assert!(Arc::ptr_eq(&self.domain, &sender.domain), "server event queue mismatch");
    }

    pub fn wait(&self, timeout: Duration) -> Wait {
        crossbeam_channel::select! {
            recv(self.events) -> event => match event {
                Ok(event) => Wait::Event(event),
                Err(_) => Wait::Disconnected,
            },
            recv(self.token) -> token => match token {
                Ok(()) => Wait::Overflow,
                // Only the ordinary channel decides shutdown.
                Err(_) => match self.events.recv_timeout(timeout) {
                    Ok(event) => Wait::Event(event),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => Wait::Timeout,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Wait::Disconnected,
                },
            },
            default(timeout) => Wait::Timeout,
        }
    }

    pub fn try_recv(&self) -> Result<Event, TryRecvError> {
        self.events.try_recv()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn reconcile(&self, selected: bool) -> Notifications {
        if !selected && self.token.try_recv().is_err() {
            return Notifications::default();
        }
        // Consume the token first; publication after this exchange leaves a later token.
        self.take_dirty()
    }

    fn take_dirty(&self) -> Notifications {
        let bits = self.domain.dirty.swap(0, Ordering::SeqCst);
        Notifications { panes: bits & PANES != 0, wake: bits & WAKE != 0 }
    }

    #[cfg(test)]
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Event, crossbeam_channel::RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(n: Notifications) -> u8 {
        (u8::from(n.panes) * PANES) | (u8::from(n.wake) * WAKE)
    }

    #[test]
    fn successful_notifications_use_only_the_ordinary_queue() {
        let (tx, rx) = channel(2);
        assert!(tx.pane_ready(7));
        assert!(tx.wake());
        assert!(matches!(rx.wait(Duration::ZERO), Wait::Event(Event::PaneReady(7))));
        assert!(matches!(rx.wait(Duration::ZERO), Wait::Event(Event::Wake)));
        assert_eq!(bits(rx.reconcile(false)), 0);
        assert!(matches!(rx.wait(Duration::ZERO), Wait::Timeout));
    }

    #[test]
    fn both_notification_kinds_coalesce_but_payloads_do_not() {
        let (tx, rx) = channel(1);
        tx.wake();
        for _ in 0..20 {
            assert!(tx.pane_ready(9));
            assert!(tx.wake());
        }
        assert_eq!(rx.token.len(), 1);
        assert!(matches!(rx.try_recv(), Ok(Event::Wake)));
        assert!(matches!(rx.wait(Duration::ZERO), Wait::Overflow));
        assert_eq!(bits(rx.reconcile(true)), PANES | WAKE);
        assert!(matches!(rx.wait(Duration::ZERO), Wait::Timeout));
        let worker = std::thread::spawn(move || {
            for i in 0..20 {
                tx.send(Event::ClientFrame(8, 0, vec![i])).unwrap();
            }
        });
        for i in 0..20 {
            assert!(
                matches!(rx.wait(Duration::from_secs(2)), Wait::Event(Event::ClientFrame(8, 0, data)) if data == [i])
            );
        }
        worker.join().unwrap();
        assert_eq!(bits(rx.reconcile(false)), 0);
    }

    #[test]
    fn own_thread_errors_return_the_event_even_with_room_or_after_shutdown() {
        for full in [false, true] {
            let (tx, rx) = channel(1);
            if full {
                tx.wake();
            }
            let event = Event::ClientFrame(3, 5, b"owned".to_vec());
            assert!(
                matches!(tx.send(event), Err(SendError::OwnThread(Event::ClientFrame(3, 5, data))) if data == b"owned")
            );
            drop(rx);
            assert!(!tx.pane_ready(3));
            assert!(!tx.wake());
            assert!(matches!(tx.send(Event::Shutdown), Err(SendError::OwnThread(Event::Shutdown))));
            std::thread::spawn(move || {
                assert!(matches!(tx.send(Event::Warning("kept".into())), Err(SendError::Disconnected(Event::Warning(w))) if w == "kept"));
            }).join().unwrap();
        }
    }

    #[test]
    fn dropping_a_receiver_releases_a_blocked_worker_with_its_payload() {
        let (tx, rx) = channel(1);
        tx.wake();
        tx.pane_ready(1);
        let worker = std::thread::spawn(move || {
            assert!(
                matches!(tx.send(Event::ClientFrame(1, 0, vec![1, 2, 3])), Err(SendError::Disconnected(Event::ClientFrame(1, 0, data))) if data == [1, 2, 3])
            );
            assert!(!tx.wake());
        });
        drop(rx);
        worker.join().unwrap();
    }

    #[test]
    fn bindings_are_distinct_even_when_recreated_on_the_same_thread() {
        let (old, rx) = channel(1);
        rx.assert_sender(&old.clone());
        old.wake();
        old.pane_ready(1);
        drop(rx);
        let (tx, rx) = channel(1);
        rx.assert_sender(&tx);
        assert!(!old.wake());
        assert!(matches!(rx.wait(Duration::ZERO), Wait::Timeout));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rx.assert_sender(&old))).is_err());
    }

    #[test]
    fn all_dirty_and_token_interleavings_remain_reachable() {
        fn schedules(prefix: &mut Vec<usize>, next: &mut [usize; 3], visit: &mut impl FnMut(&[usize])) {
            if prefix.len() == 6 {
                visit(prefix);
                return;
            }
            for actor in 0..3 {
                if next[actor] < 2 {
                    next[actor] += 1;
                    prefix.push(actor);
                    schedules(prefix, next, visit);
                    prefix.pop();
                    next[actor] -= 1;
                }
            }
        }
        let mut count = 0;
        for (token, initial) in [(false, 0), (true, 0), (true, PANES), (true, WAKE), (true, PANES | WAKE)] {
            for second in [PANES, WAKE] {
                schedules(&mut Vec::new(), &mut [0; 3], &mut |order| {
                    let (tx, rx) = channel(1);
                    tx.mark(initial);
                    if token {
                        assert!(tx.signal());
                    }
                    let mut next = [0; 3];
                    let mut selected = false;
                    let mut seen = 0;
                    for &actor in order {
                        match (actor, next[actor]) {
                            (0, 0) => tx.mark(PANES),
                            (1, 0) => tx.mark(second),
                            (0 | 1, 1) => assert!(tx.signal()),
                            (2, 0) => selected = rx.token.try_recv().is_ok(),
                            (2, 1) if selected => seen |= bits(rx.take_dirty()),
                            (2, 1) => {}
                            _ => unreachable!(),
                        }
                        next[actor] += 1;
                    }
                    // All publishers are now silent. Only the real selector may rescue work.
                    while let Wait::Overflow = rx.wait(Duration::ZERO) {
                        seen |= bits(rx.reconcile(true));
                    }
                    assert_eq!(seen, initial | PANES | second, "initial={initial}, order={order:?}");
                    assert_eq!(rx.domain.dirty.load(Ordering::SeqCst), 0);
                    count += 1;
                });
            }
        }
        assert_eq!(count, 900);
    }

    #[test]
    fn a_producer_paused_after_the_dirty_store_still_sends_a_token() {
        let (tx, rx) = channel(1);
        tx.mark(PANES);
        tx.signal();
        let (marked, resume) = crossbeam_channel::bounded(0);
        let (go, continued) = crossbeam_channel::bounded(0);
        let producer = tx.clone();
        let worker = std::thread::spawn(move || {
            producer.mark(PANES);
            marked.send(()).unwrap();
            continued.recv().unwrap();
            assert!(producer.signal());
        });
        resume.recv().unwrap();
        assert!(matches!(rx.wait(Duration::ZERO), Wait::Overflow));
        assert_eq!(bits(rx.reconcile(true)), PANES);
        go.send(()).unwrap();
        worker.join().unwrap();
        assert!(matches!(rx.wait(Duration::ZERO), Wait::Overflow));
        assert_eq!(bits(rx.reconcile(true)), 0, "a stale token is harmless");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn an_idle_selector_stays_asleep_until_publication() {
        let (bound, binding) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            crate::testing::block_signals();
            let (tx, rx) = channel(1);
            // SAFETY: gettid has no preconditions.
            bound.send((tx, unsafe { libc::gettid() })).unwrap();
            assert!(matches!(rx.wait(Duration::from_secs(30)), Wait::Event(Event::Wake)));
        });
        let (tx, tid) = binding.recv().unwrap();
        crate::testing::wait_asleep(tid);
        let switches = crate::testing::thread_switches(tid);
        // Asleep is not yet in the wait: on a busy machine the thread can block on its way there
        // (a lock, a page), so count from when its switches have stopped. A thread that polls
        // never stops, or switches again within the 700 ms that follow.
        let mut before = switches();
        let start = std::time::Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let now = switches();
            if now == before {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "the thread keeps waking up: {now} switches");
            before = now;
        }
        crate::testing::sleep_with_signals(tid, Duration::from_millis(700));
        assert_eq!(switches(), before, "no new polling interval");
        tx.wake();
        worker.join().unwrap();
    }
}
