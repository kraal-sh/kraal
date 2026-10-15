use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use anyhow::Result;
use rustix::net::{self, SocketAddrAny, SocketFlags, SocketType};

use super::{Diagnostic, TIMEOUT};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Backend {
    Wayland,
    X11,
}

pub(super) fn select<T>(
    present: impl Fn(&str) -> bool,
    mut native: impl FnMut(Backend) -> Result<Option<T>>,
    tools: impl FnOnce() -> Result<T, Diagnostic>,
) -> Result<T, Diagnostic> {
    for (variable, backend) in [("WAYLAND_DISPLAY", Backend::Wayland), ("DISPLAY", Backend::X11)] {
        if present(variable) {
            match native(backend) {
                Ok(Some(result)) => return Ok(result),
                Ok(None) => {}
                Err(error) => return Err(Diagnostic::Failed(format!("{backend:?} clipboard: {error:#}"))),
            }
        }
    }
    tools()
}

pub(super) struct Deadline<'a> {
    cancelled: &'a AtomicBool,
    until: Instant,
}

impl<'a> Deadline<'a> {
    pub fn new(cancelled: &'a AtomicBool) -> Self {
        Self { cancelled, until: Instant::now() + TIMEOUT }
    }

    pub fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(io::Error::other("clipboard read cancelled"))
        } else if Instant::now() >= self.until {
            Err(io::Error::new(io::ErrorKind::TimedOut, "clipboard read timed out; copy the image again"))
        } else {
            Ok(())
        }
    }

    pub fn poll(&self, fd: BorrowedFd<'_>, events: i16) -> io::Result<i16> {
        loop {
            self.check()?;
            let mut descriptor = libc::pollfd { fd: fd.as_raw_fd(), events, revents: 0 };
            let millis = self.until.saturating_duration_since(Instant::now()).as_millis().min(20) as i32;
            // SAFETY: descriptor points to one initialized pollfd for the duration of poll.
            let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
            if result > 0 {
                self.check()?;
                return Ok(descriptor.revents);
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    pub fn connect(&self, address: &SocketAddrAny) -> io::Result<OwnedFd> {
        self.check()?;
        let socket = net::socket_with(
            address.address_family(),
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )?;
        loop {
            self.check()?;
            match net::connect(&socket, address) {
                Ok(()) => return Ok(socket),
                Err(rustix::io::Errno::AGAIN) if address.address_family() == net::AddressFamily::UNIX => {
                    // A full Unix listener queue needs another connect attempt. Polling this
                    // unconnected socket would report HUP immediately and spin.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(rustix::io::Errno::INPROGRESS | rustix::io::Errno::INTR) => {
                    self.poll(socket.as_fd(), libc::POLLOUT)?;
                    net::sockopt::socket_error(&socket)??;
                    return Ok(socket);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub fn blocking<T: Send + 'static>(&self, work: impl FnOnce() -> Option<T> + Send + 'static) -> Result<Option<T>> {
        static WORKER: LazyLock<BlockingWorker> = LazyLock::new(BlockingWorker::default);
        WORKER.run(self, work)
    }
}

#[derive(Default)]
struct BlockingWorker(Arc<AtomicBool>);

struct Permit(Arc<AtomicBool>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl BlockingWorker {
    fn run<T: Send + 'static>(
        &self,
        deadline: &Deadline<'_>,
        work: impl FnOnce() -> Option<T> + Send + 'static,
    ) -> Result<Option<T>> {
        deadline.check()?;
        if self.0.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return Ok(None);
        }
        let permit = Permit(self.0.clone());
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        // DNS and Xauthority I/O cannot be cancelled. Keep at most one such worker,
        // even after its caller times out, and never give it a display socket to retain.
        std::thread::Builder::new().name("clipboard-connect".into()).spawn(move || {
            let result = work();
            drop(permit);
            let _ = sender.send(result);
        })?;
        loop {
            if let Err(error) = deadline.check() {
                if error.kind() == io::ErrorKind::TimedOut {
                    return Ok(None);
                }
                return Err(error.into());
            }
            match receiver.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(connection) => return Ok(connection),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_timeouts_cannot_accumulate_blocking_workers() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        let worker = BlockingWorker::default();
        let started = Arc::new(AtomicUsize::new(0));
        let (release, blocked) = std::sync::mpsc::channel();
        let (entered, running) = std::sync::mpsc::channel();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let started = started.clone();
            let caller = scope.spawn(|| {
                let deadline = Deadline { cancelled: &stop, until: Instant::now() + crate::testing::PATIENCE };
                worker.run(&deadline, move || {
                    started.fetch_add(1, Ordering::SeqCst);
                    entered.send(()).unwrap();
                    blocked.recv().unwrap();
                    Some(())
                })
            });
            running.recv_timeout(crate::testing::PATIENCE).unwrap();
            stop.store(true, Ordering::SeqCst);
            assert!(caller.join().unwrap().unwrap_err().to_string().contains("cancelled"));
        });
        stop.store(false, Ordering::SeqCst);
        for _ in 0..20 {
            let started = started.clone();
            let deadline = Deadline { cancelled: &stop, until: Instant::now() + Duration::from_millis(20) };
            assert!(
                worker
                    .run(&deadline, move || {
                        started.fetch_add(1, Ordering::SeqCst);
                        Some(())
                    })
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(started.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        let until = Instant::now() + crate::testing::PATIENCE;
        while worker.0.load(Ordering::SeqCst) {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        // A timed-out job also retains the slot until it actually finishes.
        let (release, blocked) = std::sync::mpsc::channel();
        let deadline = Deadline { cancelled: &stop, until: Instant::now() + Duration::from_millis(30) };
        assert!(
            worker
                .run(&deadline, move || {
                    blocked.recv().unwrap();
                    Some(())
                })
                .unwrap()
                .is_none()
        );
        for _ in 0..20 {
            assert!(
                worker
                    .run::<()>(&Deadline::new(&stop), || panic!("the timed-out worker is still alive"))
                    .unwrap()
                    .is_none()
            );
        }
        release.send(()).unwrap();
        while worker.0.load(Ordering::SeqCst) {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(worker.run(&Deadline::new(&stop), || Some(42)).unwrap(), Some(42));
    }

    #[test]
    fn full_listener_queue_times_out_repeatedly_and_cancels_without_pending_connections() {
        use std::time::Duration;

        let dir = crate::testing::TempDir::new("clipboard-listener");
        let address = net::SocketAddrUnix::new(dir.0.join("socket")).unwrap().into();
        let listener = net::socket_with(
            net::AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        net::bind(&listener, &address).unwrap();
        net::listen(&listener, 0).unwrap();
        let stop = AtomicBool::new(false);
        let _queued = Deadline::new(&stop).connect(&address).unwrap();
        for _ in 0..8 {
            let deadline = Deadline { cancelled: &stop, until: Instant::now() + Duration::from_millis(20) };
            assert_eq!(deadline.connect(&address).unwrap_err().kind(), io::ErrorKind::TimedOut);
        }
        std::thread::scope(|scope| {
            let connecting = scope.spawn(|| Deadline::new(&stop).connect(&address));
            std::thread::sleep(Duration::from_millis(20));
            stop.store(true, Ordering::SeqCst);
            assert!(connecting.join().unwrap().unwrap_err().to_string().contains("cancelled"));
        });
        drop(net::accept(&listener).unwrap());
        assert_eq!(net::accept(&listener).unwrap_err(), rustix::io::Errno::AGAIN);
        stop.store(false, Ordering::SeqCst);
        let _connected = Deadline::new(&stop).connect(&address).unwrap();
        assert!(net::accept(&listener).is_ok());
    }

    #[test]
    fn backends_are_ordered_and_tools_require_both_native_paths_to_be_unavailable() {
        for (wayland, x11, expected) in [
            (Some(true), Some(false), vec![Backend::Wayland]),
            (Some(false), Some(true), vec![Backend::Wayland]),
            (None, Some(true), vec![Backend::Wayland, Backend::X11]),
            (None, Some(false), vec![Backend::Wayland, Backend::X11]),
            (None, None, vec![Backend::Wayland, Backend::X11]),
        ] {
            let mut attempted = Vec::new();
            let mut used_tools = false;
            let result = select(
                |_| true,
                |backend| {
                    attempted.push(backend);
                    Ok(match backend {
                        Backend::Wayland => wayland,
                        Backend::X11 => x11,
                    })
                },
                || {
                    used_tools = true;
                    Ok(true)
                },
            )
            .unwrap();
            assert_eq!(attempted, expected);
            assert_eq!(used_tools, wayland.is_none() && x11.is_none());
            assert_eq!(result, wayland.or(x11).unwrap_or(true));
        }
        for failing in [Backend::Wayland, Backend::X11] {
            let result = select(
                |_| true,
                |backend| if backend == failing { anyhow::bail!("owner gone") } else { Ok(None::<bool>) },
                || panic!("a native failure must not use tools"),
            );
            assert!(matches!(result, Err(Diagnostic::Failed(error)) if error.contains("owner gone")));
        }
    }

    #[test]
    fn absent_displays_are_skipped() {
        let result = select(|_| false, |_| panic!("no display"), || Err::<bool, _>(Diagnostic::NoDisplay));
        assert!(matches!(result, Err(Diagnostic::NoDisplay)));
        let result = select(
            |variable| variable == "DISPLAY",
            |backend| {
                assert_eq!(backend, Backend::X11);
                Ok(Some(true))
            },
            || panic!("X11 connected"),
        );
        assert!(result.unwrap());
    }

    #[test]
    fn waiting_checks_cancellation_and_timeout_without_a_display() {
        use std::os::fd::AsFd;
        let (reader, _writer) = std::io::pipe().unwrap();
        let stop = AtomicBool::new(true);
        assert!(Deadline::new(&stop).poll(reader.as_fd(), libc::POLLIN).unwrap_err().to_string().contains("cancelled"));
        stop.store(false, Ordering::SeqCst);
        let deadline = Deadline { cancelled: &stop, until: Instant::now() + std::time::Duration::from_millis(30) };
        assert_eq!(deadline.poll(reader.as_fd(), libc::POLLIN).unwrap_err().kind(), io::ErrorKind::TimedOut);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(20));
                stop.store(true, Ordering::SeqCst);
            });
            assert!(
                Deadline::new(&stop).poll(reader.as_fd(), libc::POLLIN).unwrap_err().to_string().contains("cancelled")
            );
        });
    }
}
