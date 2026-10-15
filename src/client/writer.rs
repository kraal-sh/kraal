//! Bound writes to SSH's pipe, including when SSH stops reading it.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(super) struct TimedWriter<W> {
    pipe: W,
    timeout: Duration,
    child: Option<Arc<Mutex<std::process::Child>>>,
    failed: bool,
}

impl<W: Write + AsRawFd> TimedWriter<W> {
    pub fn new(pipe: W, timeout: Duration, child: Option<Arc<Mutex<std::process::Child>>>) -> io::Result<Self> {
        let fd = pipe.as_raw_fd();
        // SAFETY: the pipe owns this descriptor. Only this writer writes to it.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { pipe, timeout, child, failed: false })
    }

    fn write_until(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + self.timeout;
        while !bytes.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "SSH stopped reading input"));
            }
            match self.pipe.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut fd = libc::pollfd { fd: self.pipe.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                    let millis = left.as_millis().clamp(1, i32::MAX as u128) as i32;
                    // SAFETY: one valid pollfd for the pipe this writer owns.
                    if unsafe { libc::poll(&mut fd, 1, millis) } < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl<W: Write + AsRawFd> Write for TimedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if let Err(error) = self.write_until(bytes) {
            self.failed = true;
            if let Some(child) = &self.child {
                // Share the child handle with Conn: killing after it was reaped is harmless.
                // Ending SSH also wakes the connection reader so attach can reconnect.
                let _ = child.lock().unwrap().kill();
            }
            return Err(error);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pipe.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_stalled_pipe_times_out_and_never_continues_a_partial_frame() {
        let (mut reader, pipe) = std::io::pipe().unwrap();
        let mut writer = TimedWriter::new(pipe, Duration::from_millis(20), None).unwrap();
        let start = Instant::now();
        let error = writer.write_all(&vec![b'x'; 8 * 1024 * 1024]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < crate::testing::PATIENCE);
        assert_eq!(writer.write(b"another frame").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        drop(writer);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.iter().all(|&b| b == b'x'));
    }
}
