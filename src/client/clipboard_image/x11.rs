use std::io;
use std::net::ToSocketAddrs;
use std::os::fd::AsFd;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, ensure};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, Property, WindowClass};
use x11rb::reexports::x11rb_protocol::{
    parse_display::{ConnectAddress, parse_display},
    xauth::{Family, get_auth},
};
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use super::native::Deadline;
use super::{MAX_BYTES, TOO_LARGE};

pub(super) fn list_types(cancelled: &AtomicBool) -> Result<Option<bool>> {
    let deadline = Deadline::new(cancelled);
    Clipboard::connect(&deadline)?.map(|clipboard| clipboard.list_types()).transpose()
}

pub(super) fn read_png(cancelled: &AtomicBool) -> Result<Option<Vec<u8>>> {
    let deadline = Deadline::new(cancelled);
    let Some(clipboard) = Clipboard::connect(&deadline)? else { return Ok(None) };
    ensure!(clipboard.list_types()?, "clipboard has no PNG image; copy an image first");
    Ok(Some(clipboard.convert(clipboard.png, clipboard.png, 8, MAX_BYTES)?))
}

pub(super) fn read_text(cancelled: &AtomicBool, limit: usize) -> Result<Option<Vec<u8>>> {
    let deadline = Deadline::new(cancelled);
    let Some(clipboard) = Clipboard::connect(&deadline)? else { return Ok(None) };
    if clipboard.connection.get_selection_owner(clipboard.clipboard)?.reply()?.owner == NONE {
        return Ok(Some(Vec::new()));
    }
    let utf8 = clipboard.connection.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
    let targets = clipboard.convert(clipboard.targets, AtomEnum::ATOM.into(), 32, 16 * 1024)?;
    let Some(target) = text_target(&targets, utf8)? else { return Ok(Some(Vec::new())) };
    let bytes = clipboard.convert(target, target, 8, limit).map_err(|error| {
        if error.to_string() == size_error(limit) { anyhow::anyhow!(super::TEXT_TOO_LARGE) } else { error }
    })?;
    Ok(Some(decode_text(bytes, target == Atom::from(AtomEnum::STRING), limit)?))
}

fn text_target(bytes: &[u8], utf8: Atom) -> Result<Option<Atom>> {
    for target in [utf8, AtomEnum::STRING.into()] {
        if offers_png(bytes, target)? {
            return Ok(Some(target));
        }
    }
    Ok(None)
}

fn decode_text(bytes: Vec<u8>, latin1: bool, limit: usize) -> Result<Vec<u8>> {
    let bytes = if latin1 { bytes.into_iter().map(char::from).collect::<String>().into_bytes() } else { bytes };
    ensure!(bytes.len() <= limit, super::TEXT_TOO_LARGE);
    Ok(bytes)
}

struct TimedStream<'a, 'b> {
    stream: DefaultStream,
    deadline: &'a Deadline<'b>,
}

impl Stream for TimedStream<'_, '_> {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let events = if mode.readable() { libc::POLLIN } else { 0 } | if mode.writable() { libc::POLLOUT } else { 0 };
        self.deadline.poll(self.stream.as_fd(), events).map(|_| ())
    }

    fn read(&self, buf: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.deadline.check()?;
        self.stream.read(buf, fds)
    }

    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.deadline.check()?;
        self.stream.write(buf, fds)
    }
}

struct Clipboard<'a, 'b> {
    connection: RustConnection<TimedStream<'a, 'b>>,
    window: u32,
    clipboard: Atom,
    targets: Atom,
    png: Atom,
    incr: Atom,
    property: Atom,
}

type Transport = (DefaultStream, (Family, Vec<u8>));

fn connect_transport(deadline: &Deadline<'_>, address: ConnectAddress<'_>) -> Result<Option<Transport>> {
    match address {
        ConnectAddress::Socket(path) => {
            let mut addresses = Vec::new();
            #[cfg(any(target_os = "linux", target_os = "android"))]
            addresses.push(rustix::net::SocketAddrUnix::new_abstract_name(path.as_bytes())?);
            addresses.push(rustix::net::SocketAddrUnix::new(path)?);
            for address in addresses {
                if let Ok(socket) = deadline.connect(&address.into()) {
                    return Ok(Some(DefaultStream::from_unix_stream(socket.into())?));
                }
                deadline.check()?;
            }
        }
        ConnectAddress::Hostname(host, port) => {
            let host = host.to_owned();
            let Some(addresses) = deadline.blocking(move || {
                (host.as_str(), port).to_socket_addrs().ok().map(|addresses| addresses.collect::<Vec<_>>())
            })?
            else {
                return Ok(None);
            };
            for address in addresses {
                if let Ok(socket) = deadline.connect(&address.into()) {
                    return Ok(Some(DefaultStream::from_tcp_stream(socket.into())?));
                }
                deadline.check()?;
            }
        }
        _ => {}
    }
    Ok(None)
}

impl<'a, 'b> Clipboard<'a, 'b> {
    fn connect(deadline: &'a Deadline<'b>) -> Result<Option<Self>> {
        let Ok(display) = parse_display(None) else { return Ok(None) };
        let mut transport = None;
        for address in display.connect_instruction() {
            if let Some(connection) = connect_transport(deadline, address)? {
                transport = Some(connection);
                break;
            }
        }
        let Some((stream, (family, address))) = transport else { return Ok(None) };
        let auth = deadline
            .blocking(move || Some(get_auth(family, &address, display.display).ok().flatten().unwrap_or_default()))?;
        let Some((auth_name, auth_data)) = auth else { return Ok(None) };
        let screen = usize::from(display.screen);
        let connection = RustConnection::connect_to_stream_with_auth_info(
            TimedStream { stream, deadline },
            screen,
            auth_name,
            auth_data,
        )?;
        let intern = |name: &[u8]| -> Result<Atom> { Ok(connection.intern_atom(false, name)?.reply()?.atom) };
        let (clipboard, targets, png, incr, property) = (
            intern(b"CLIPBOARD")?,
            intern(b"TARGETS")?,
            intern(b"image/png")?,
            intern(b"INCR")?,
            intern(b"KRAAL_CLIPBOARD")?,
        );
        let window = connection.generate_id()?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                connection.setup().roots[screen].root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?
            .check()?;
        Ok(Some(Self { connection, window, clipboard, targets, png, incr, property }))
    }

    fn list_types(&self) -> Result<bool> {
        if self.connection.get_selection_owner(self.clipboard)?.reply()?.owner == NONE {
            return Ok(false);
        }
        let bytes = self.convert(self.targets, AtomEnum::ATOM.into(), 32, 16 * 1024)?;
        offers_png(&bytes, self.png)
    }

    fn convert(&self, target: Atom, kind: Atom, format: u8, limit: usize) -> Result<Vec<u8>> {
        let connection = &self.connection;
        connection.delete_property(self.window, self.property)?;
        connection.convert_selection(self.window, self.clipboard, target, self.property, CURRENT_TIME)?;
        connection.flush()?;
        loop {
            connection.stream().deadline.check()?;
            if let Event::SelectionNotify(event) = connection.wait_for_event()?
                && event.requestor == self.window
                && event.selection == self.clipboard
                && event.target == target
            {
                ensure!(event.property == self.property, "clipboard owner refused the requested type or disappeared");
                break;
            }
        }
        let initial = self.property(limit, limit)?;
        // INCR owners wait for each property deletion before sending the next chunk.
        // Discard the initial PropertyNotify before acknowledging the INCR header.
        while connection.poll_for_event()?.is_some() {
            connection.stream().deadline.check()?;
        }
        let result = reassemble(initial, self.incr, kind, format, limit, |remaining| {
            connection.delete_property(self.window, self.property)?;
            connection.flush()?;
            loop {
                connection.stream().deadline.check()?;
                if let Event::PropertyNotify(event) = connection.wait_for_event()?
                    && event.window == self.window
                    && event.atom == self.property
                    && event.state == Property::NEW_VALUE
                {
                    return self.property(remaining, limit);
                }
            }
        });
        connection.delete_property(self.window, self.property)?;
        connection.flush()?;
        result
    }

    fn property(&self, remaining: usize, limit: usize) -> Result<Value> {
        read_property(remaining, limit, |length| {
            Ok(self.connection.get_property(false, self.window, self.property, AtomEnum::ANY, 0, length)?.reply()?)
        })
    }
}

impl Drop for Clipboard<'_, '_> {
    fn drop(&mut self) {
        let _ = self.connection.delete_property(self.window, self.property);
        let _ = self.connection.destroy_window(self.window);
        let _ = self.connection.flush();
    }
}

struct Value {
    kind: Atom,
    format: u8,
    bytes: Vec<u8>,
}

fn read_property(
    remaining: usize,
    limit: usize,
    mut fetch: impl FnMut(u32) -> Result<x11rb::protocol::xproto::GetPropertyReply>,
) -> Result<Value> {
    let metadata = fetch(0)?;
    ensure!(metadata.bytes_after as usize <= remaining, size_error(limit));
    let reply = fetch(metadata.bytes_after.div_ceil(4))?;
    ensure!(reply.bytes_after == 0 && reply.value.len() <= remaining, size_error(limit));
    ensure!(
        reply.type_ == metadata.type_ && reply.format == metadata.format,
        "clipboard property changed during the read"
    );
    Ok(Value { kind: reply.type_, format: reply.format, bytes: reply.value })
}

fn size_error(limit: usize) -> &'static str {
    if limit == 16 * 1024 { "clipboard type list exceeds 16 KiB" } else { TOO_LARGE }
}

fn offers_png(bytes: &[u8], png: Atom) -> Result<bool> {
    ensure!(bytes.len().is_multiple_of(4), "invalid clipboard TARGETS property");
    Ok(bytes.as_chunks::<4>().0.iter().any(|atom| u32::from_ne_bytes(*atom) == png))
}

fn reassemble(
    initial: Value,
    incr: Atom,
    kind: Atom,
    format: u8,
    limit: usize,
    mut next: impl FnMut(usize) -> Result<Value>,
) -> Result<Vec<u8>> {
    let valid = |value: &Value| -> Result<()> {
        ensure!(
            value.kind == kind && value.format == format,
            "clipboard returned an unexpected property type or format"
        );
        Ok(())
    };
    if initial.kind != incr {
        valid(&initial)?;
        ensure!(initial.bytes.len() <= limit, size_error(limit));
        return Ok(initial.bytes);
    }
    ensure!(initial.format == 32 && initial.bytes.len() == 4, "invalid clipboard INCR header");
    let minimum = u32::from_ne_bytes(initial.bytes[..4].try_into().unwrap()) as usize;
    ensure!(minimum <= limit, size_error(limit));
    let mut bytes = Vec::new();
    loop {
        let chunk = next(limit - bytes.len())?;
        valid(&chunk)?;
        ensure!(chunk.bytes.len() <= limit - bytes.len(), size_error(limit));
        if chunk.bytes.is_empty() {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INCR: Atom = 1;
    const PNG: Atom = 2;

    fn value(bytes: Vec<u8>) -> Value {
        Value { kind: PNG, format: 8, bytes }
    }
    fn header(minimum: u32) -> Value {
        Value { kind: INCR, format: 32, bytes: minimum.to_ne_bytes().to_vec() }
    }

    #[test]
    fn text_targets_prefer_utf8_and_string_is_latin1_with_a_utf8_size_limit() {
        let utf8: Atom = 999;
        let string: Atom = AtomEnum::STRING.into();
        let targets = [string.to_ne_bytes(), utf8.to_ne_bytes()].concat();
        assert_eq!(text_target(&targets, utf8).unwrap(), Some(utf8));
        assert_eq!(text_target(&string.to_ne_bytes(), utf8).unwrap(), Some(string));
        assert_eq!(text_target(&[], utf8).unwrap(), None);
        assert!(text_target(&[1], utf8).is_err());
        assert_eq!(decode_text(vec![b'a', 0xe9, b'\n'], true, 4).unwrap(), "aé\n".as_bytes());
        assert_eq!(decode_text("aé\n".as_bytes().to_vec(), false, 4).unwrap(), "aé\n".as_bytes());
        assert_eq!(decode_text(vec![0xe9], true, 1).unwrap_err().to_string(), super::super::TEXT_TOO_LARGE);
    }

    #[test]
    fn targets_are_atoms_not_text_and_must_be_complete() {
        assert!(offers_png(&[7u32.to_ne_bytes(), PNG.to_ne_bytes()].concat(), PNG).unwrap());
        assert!(!offers_png(&7u32.to_ne_bytes(), PNG).unwrap());
        assert!(!offers_png(&[], PNG).unwrap());
        assert!(offers_png(&[1, 2, 3], PNG).is_err());
        assert!(offers_png(&[PNG.to_ne_bytes().as_slice(), &[1, 2, 3]].concat(), PNG).is_err());
    }

    #[test]
    fn property_size_is_checked_before_fetching_any_content() {
        use x11rb::protocol::xproto::GetPropertyReply;
        for remaining in [0, 15, MAX_BYTES] {
            let mut calls = 0;
            let result = read_property(remaining, MAX_BYTES, |length| {
                calls += 1;
                assert_eq!(length, 0);
                Ok(GetPropertyReply { format: 8, type_: PNG, bytes_after: remaining as u32 + 1, ..Default::default() })
            });
            assert!(result.err().unwrap().to_string().contains("16 MiB"));
            assert_eq!(calls, 1);
        }
        let mut lengths = Vec::new();
        let result = read_property(5, MAX_BYTES, |length| {
            lengths.push(length);
            Ok(GetPropertyReply {
                format: 8,
                type_: PNG,
                bytes_after: if length == 0 { 5 } else { 0 },
                value: if length == 0 { vec![] } else { b"image".to_vec() },
                ..Default::default()
            })
        })
        .unwrap();
        assert_eq!(result.bytes, b"image");
        assert_eq!(lengths, [0, 2]);
    }

    #[test]
    fn incr_reassembles_chunks_until_the_empty_property() {
        let mut chunks = [b"first".to_vec(), b"second".to_vec(), Vec::new()].into_iter();
        let mut budgets = Vec::new();
        let bytes = reassemble(header(0), INCR, PNG, 8, MAX_BYTES, |remaining| {
            budgets.push(remaining);
            Ok(value(chunks.next().unwrap()))
        })
        .unwrap();
        assert_eq!(bytes, b"firstsecond");
        assert_eq!(budgets, [MAX_BYTES, MAX_BYTES - 5, MAX_BYTES - 11]);
        assert!(chunks.next().is_none());
        assert_eq!(
            reassemble(value(b"direct".to_vec()), INCR, PNG, 8, MAX_BYTES, |_| panic!("not INCR")).unwrap(),
            b"direct"
        );
    }

    #[test]
    fn incr_rejects_announced_and_actual_overflow_and_accepts_exact_limit() {
        assert!(
            reassemble(header(MAX_BYTES as u32 + 1), INCR, PNG, 8, MAX_BYTES, |_| panic!("oversized announcement"))
                .unwrap_err()
                .to_string()
                .contains("16 MiB")
        );
        for overflow in [false, true] {
            let mut calls = 0;
            let result = reassemble(header(1), INCR, PNG, 8, MAX_BYTES, |remaining| {
                calls += 1;
                match calls {
                    1 => {
                        assert_eq!(remaining, MAX_BYTES);
                        Ok(value(vec![0; MAX_BYTES]))
                    }
                    2 => {
                        assert_eq!(remaining, 0);
                        if overflow {
                            anyhow::bail!(TOO_LARGE);
                        }
                        Ok(value(Vec::new()))
                    }
                    _ => panic!("read after termination"),
                }
            });
            if overflow {
                assert!(result.unwrap_err().to_string().contains("16 MiB"));
            } else {
                assert_eq!(result.unwrap().len(), MAX_BYTES);
            }
        }
    }

    #[test]
    fn incr_rejects_bad_headers_changed_types_and_reader_failures() {
        let mut bad_header = header(0);
        bad_header.format = 8;
        assert!(reassemble(bad_header, INCR, PNG, 8, MAX_BYTES, |_| panic!("bad header")).is_err());
        assert!(
            reassemble(header(0), INCR, PNG, 8, MAX_BYTES, |_| Ok(Value { kind: 3, format: 8, bytes: vec![] }))
                .is_err()
        );
        for reason in ["clipboard read cancelled", "clipboard read timed out", "owner gone"] {
            assert!(
                reassemble(header(0), INCR, PNG, 8, MAX_BYTES, |_| anyhow::bail!(reason))
                    .unwrap_err()
                    .to_string()
                    .contains(reason)
            );
        }
    }
}
