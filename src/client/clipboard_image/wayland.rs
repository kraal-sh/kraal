use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, ensure};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as ext_device, ext_data_control_manager_v1 as ext_manager,
    ext_data_control_offer_v1 as ext_offer,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as wlr_device, zwlr_data_control_manager_v1 as wlr_manager,
    zwlr_data_control_offer_v1 as wlr_offer,
};

use super::native::Deadline;
use super::{MAX_BYTES, TOO_LARGE};

pub(super) fn list_types(cancelled: &AtomicBool) -> Result<Option<bool>> {
    list_types_with(cancelled, |name| std::env::var_os(name))
}

fn list_types_with(cancelled: &AtomicBool, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Option<bool>> {
    let deadline = Deadline::new(cancelled);
    Ok(Clipboard::connect(&deadline, env)?.map(|clipboard| clipboard.has_png()))
}

pub(super) fn read_png(cancelled: &AtomicBool) -> Result<Option<Vec<u8>>> {
    read_png_with(cancelled, |name| std::env::var_os(name))
}

fn read_png_with(cancelled: &AtomicBool, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Option<Vec<u8>>> {
    let deadline = Deadline::new(cancelled);
    let Some(clipboard) = Clipboard::connect(&deadline, env)? else { return Ok(None) };
    ensure!(clipboard.has_png(), "clipboard has no PNG image; copy an image first");
    let (reader, writer) = io::pipe()?;
    match clipboard.state.selection.as_ref().and_then(Option::as_ref).unwrap() {
        Offer::Ext(offer) => offer.receive("image/png".into(), writer.as_fd()),
        Offer::Wlr(offer) => offer.receive("image/png".into(), writer.as_fd()),
    }
    drop(writer);
    flush(&clipboard.connection, &deadline)?;
    Ok(Some(read_pipe(reader, &deadline)?))
}

fn read_pipe(mut reader: io::PipeReader, deadline: &Deadline<'_>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        deadline.poll(reader.as_fd(), libc::POLLIN)?;
        let remaining = MAX_BYTES - bytes.len();
        if remaining == 0 {
            let mut pending: libc::c_int = 0;
            // SAFETY: FIONREAD writes one integer and reader owns a valid pipe descriptor.
            ensure!(
                unsafe { libc::ioctl(reader.as_raw_fd(), libc::FIONREAD, &mut pending) } == 0,
                "cannot inspect clipboard pipe"
            );
            ensure!(pending == 0, TOO_LARGE);
            return Ok(bytes);
        }
        let length = remaining.min(buffer.len());
        match reader.read(&mut buffer[..length]) {
            Ok(0) => return Ok(bytes),
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
}

struct Clipboard {
    connection: Connection,
    state: State,
}

impl Clipboard {
    fn connect(deadline: &Deadline<'_>, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Option<Self>> {
        let Some(connection) = connect(deadline, env)? else { return Ok(None) };
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let mut state = State::default();
        connection.display().sync(&qh, ());
        while !state.synced {
            dispatch(&connection, &mut queue, &mut state, deadline)?;
        }
        // GNOME exposes no data-control manager, so its clipboard is read through XWayland.
        let Some((name, ext)) = manager(&state.globals) else { return Ok(None) };
        let seat = state.globals.iter().find(|(_, interface, _)| interface == "wl_seat");
        let Some(&(seat, _, _)) = seat else { anyhow::bail!("Wayland has no clipboard seat") };
        let seat = registry.bind::<wl_seat::WlSeat, _, _>(seat, 1, &qh, ());
        if ext {
            registry.bind::<ext_manager::ExtDataControlManagerV1, _, _>(name, 1, &qh, ()).get_data_device(
                &seat,
                &qh,
                (),
            );
        } else {
            registry.bind::<wlr_manager::ZwlrDataControlManagerV1, _, _>(name, 2, &qh, ()).get_data_device(
                &seat,
                &qh,
                (),
            );
        }
        while state.selection.is_none() {
            dispatch(&connection, &mut queue, &mut state, deadline)?;
        }
        Ok(Some(Self { connection, state }))
    }

    fn has_png(&self) -> bool {
        self.state
            .selection
            .as_ref()
            .and_then(Option::as_ref)
            .and_then(|offer| self.state.types.get(&offer.id()))
            .is_some_and(|types| offers_png(&types.mime))
    }
}

fn connect(deadline: &Deadline<'_>, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Option<Connection>> {
    // Each probe and read owns a fresh connection. WAYLAND_SOCKET belongs to the
    // process that inherited it; connect_to_env would consume it and mutate the environment.
    let Some(display) = env("WAYLAND_DISPLAY") else { return Ok(None) };
    let mut path = PathBuf::from(display);
    if !path.is_absolute() {
        let Some(runtime) = env("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|path| path.is_absolute()) else {
            return Ok(None);
        };
        path = runtime.join(path);
    }
    let Ok(address) = rustix::net::SocketAddrUnix::new(&path) else { return Ok(None) };
    match deadline.connect(&address.into()) {
        Ok(socket) => Ok(Connection::from_socket(socket.into()).ok()),
        Err(_) => {
            deadline.check()?;
            Ok(None)
        }
    }
}

fn offers_png(types: &[String]) -> bool {
    types.iter().any(|mime| mime == "image/png")
}

fn manager(globals: &[(u32, String, u32)]) -> Option<(u32, bool)> {
    globals
        .iter()
        .find(|(_, name, version)| name == "ext_data_control_manager_v1" && *version >= 1)
        .map(|(name, _, _)| (*name, true))
        .or_else(|| {
            globals
                .iter()
                .find(|(_, name, version)| name == "zwlr_data_control_manager_v1" && *version >= 2)
                .map(|(name, _, _)| (*name, false))
        })
}

fn flush(connection: &Connection, deadline: &Deadline<'_>) -> Result<()> {
    loop {
        deadline.check()?;
        match connection.flush() {
            Ok(()) => return Ok(()),
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                deadline.poll(connection.as_fd(), libc::POLLOUT)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn dispatch(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: &Deadline<'_>,
) -> Result<()> {
    deadline.check()?;
    flush(connection, deadline)?;
    if let Some(guard) = queue.prepare_read() {
        deadline.poll(connection.as_fd(), libc::POLLIN)?;
        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
    }
    queue.dispatch_pending(state)?;
    ensure!(!state.finished, "Wayland clipboard device is no longer available");
    let types = state.selection.as_ref().and_then(Option::as_ref).and_then(|offer| state.types.get(&offer.id()));
    ensure!(types.is_none_or(|types| types.bytes <= 16 * 1024), "clipboard type list exceeds 16 KiB");
    Ok(())
}

#[derive(Default)]
struct State {
    globals: Vec<(u32, String, u32)>,
    synced: bool,
    types: HashMap<ObjectId, Types>,
    selection: Option<Option<Offer>>,
    finished: bool,
}

#[derive(Default)]
struct Types {
    mime: Vec<String>,
    bytes: usize,
}

enum Offer {
    Ext(ext_offer::ExtDataControlOfferV1),
    Wlr(wlr_offer::ZwlrDataControlOfferV1),
}

impl Offer {
    fn id(&self) -> ObjectId {
        match self {
            Self::Ext(offer) => offer.id(),
            Self::Wlr(offer) => offer.id(),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

impl State {
    fn offer(&mut self, id: ObjectId, mime: String) {
        let types = self.types.entry(id).or_default();
        types.bytes = types.bytes.saturating_add(mime.len() + 1);
        if types.bytes <= 16 * 1024 {
            types.mime.push(mime);
        }
    }

    fn discard(&mut self, offer: Offer) {
        self.types.remove(&offer.id());
        offer.destroy();
    }

    fn select(&mut self, offer: Option<Offer>) {
        if let Some(Some(previous)) = self.selection.take() {
            self.discard(previous);
        }
        self.selection = Some(offer);
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            state.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced = true;
    }
}

impl Dispatch<ext_offer::ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ext_offer::ExtDataControlOfferV1,
        event: ext_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_offer::Event::Offer { mime_type } = event {
            state.offer(offer.id(), mime_type);
        }
    }
}

impl Dispatch<wlr_offer::ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &wlr_offer::ZwlrDataControlOfferV1,
        event: wlr_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wlr_offer::Event::Offer { mime_type } = event {
            state.offer(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ext_device::ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_device::ExtDataControlDeviceV1,
        event: ext_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_device::Event::Selection { id } => state.select(id.map(Offer::Ext)),
            ext_device::Event::PrimarySelection { id: Some(offer) } => state.discard(Offer::Ext(offer)),
            ext_device::Event::Finished => state.finished = true,
            _ => {}
        }
    }
    event_created_child!(State, ext_device::ExtDataControlDeviceV1, [0 => (ext_offer::ExtDataControlOfferV1, ())]);
}

impl Dispatch<wlr_device::ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &wlr_device::ZwlrDataControlDeviceV1,
        event: wlr_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wlr_device::Event::Selection { id } => state.select(id.map(Offer::Wlr)),
            wlr_device::Event::PrimarySelection { id: Some(offer) } => state.discard(Offer::Wlr(offer)),
            wlr_device::Event::Finished => state.finished = true,
            _ => {}
        }
    }
    event_created_child!(State, wlr_device::ZwlrDataControlDeviceV1, [0 => (wlr_offer::ZwlrDataControlOfferV1, ())]);
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ext_manager::ExtDataControlManagerV1);
delegate_noop!(State: ignore wlr_manager::ZwlrDataControlManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_probe_and_read_leave_the_inherited_socket_alone() {
        use std::io::Write;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::atomic::Ordering;
        use std::sync::{Barrier, Mutex};

        let dir = crate::testing::TempDir::new("wayland-connections");
        let probe_listener = UnixListener::bind(dir.0.join("probe")).unwrap();
        let read_listener = UnixListener::bind(dir.0.join("read")).unwrap();
        for listener in [&probe_listener, &read_listener] {
            listener.set_nonblocking(true).unwrap();
        }
        let (mut inherited, mut inherited_peer) = UnixStream::pair().unwrap();
        let requested = Mutex::new(Vec::new());
        let env = |display: &str, name: &str| {
            requested.lock().unwrap().push(name.to_owned());
            match name {
                "WAYLAND_DISPLAY" => Some(display.into()),
                "XDG_RUNTIME_DIR" => Some(dir.0.clone().into_os_string()),
                "WAYLAND_SOCKET" => Some(inherited.as_raw_fd().to_string().into()),
                _ => panic!("unexpected environment variable"),
            }
        };
        let cancel_probe = AtomicBool::new(false);
        let cancel_read = AtomicBool::new(false);
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let probe = scope.spawn(|| {
                barrier.wait();
                list_types_with(&cancel_probe, |name| env("probe", name))
            });
            let read = scope.spawn(|| {
                barrier.wait();
                read_png_with(&cancel_read, |name| env("read", name))
            });
            let accept = |listener: &UnixListener| {
                Deadline::new(&cancel_read).poll(listener.as_fd(), libc::POLLIN).unwrap();
                let (mut peer, _) = listener.accept().unwrap();
                peer.set_read_timeout(Some(crate::testing::PATIENCE)).unwrap();
                let mut requests = [0; 24];
                peer.read_exact(&mut requests).unwrap();
                // Both operations requested the registry and a sync on their own socket.
                assert_eq!(u32::from_ne_bytes(requests[4..8].try_into().unwrap()), (12 << 16) | 1);
                assert_eq!(u32::from_ne_bytes(requests[16..20].try_into().unwrap()), 12 << 16);
                (peer, u32::from_ne_bytes(requests[20..24].try_into().unwrap()))
            };
            let (mut probe_peer, _) = accept(&probe_listener);
            let (mut read_peer, callback) = accept(&read_listener);
            cancel_probe.store(true, Ordering::SeqCst);
            assert!(probe.join().unwrap().unwrap_err().to_string().contains("cancelled"));
            assert_eq!(probe_peer.read(&mut [0]).unwrap(), 0);
            // Completing the other operation must still work after the probe has closed.
            // No data-control globals means the read can fall back to X11.
            read_peer
                .write_all(&[callback.to_ne_bytes(), (12u32 << 16).to_ne_bytes(), 0u32.to_ne_bytes()].concat())
                .unwrap();
            assert!(read.join().unwrap().unwrap().is_none());
        });
        assert!(!requested.lock().unwrap().iter().any(|name| name == "WAYLAND_SOCKET"));
        inherited.write_all(b"still owned").unwrap();
        let mut bytes = [0; 11];
        inherited_peer.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"still owned");
    }

    #[test]
    fn ext_is_preferred_and_wlr_requires_version_two() {
        let globals = |entries: &[(&str, u32)]| {
            entries
                .iter()
                .enumerate()
                .map(|(i, (name, version))| (i as u32, (*name).into(), *version))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            manager(&globals(&[("zwlr_data_control_manager_v1", 2), ("ext_data_control_manager_v1", 1)])),
            Some((1, true))
        );
        assert_eq!(manager(&globals(&[("zwlr_data_control_manager_v1", 3)])), Some((0, false)));
        assert_eq!(manager(&globals(&[("zwlr_data_control_manager_v1", 1), ("wl_data_device_manager", 3)])), None);
    }

    #[test]
    fn mime_types_require_an_exact_png_offer_and_are_bounded() {
        for types in [vec![], vec!["text/plain".into()], vec!["image/png-extra".into()], vec!["image/jpeg".into()]] {
            assert!(!offers_png(&types));
        }
        assert!(offers_png(&["text/plain".into(), "image/png".into()]));
        let mut state = State::default();
        let id = ObjectId::null();
        state.offer(id.clone(), "image/png".into());
        state.offer(id.clone(), "x".repeat(16 * 1024));
        assert!(state.types[&id].bytes > 16 * 1024);
        assert_eq!(state.types[&id].mime, ["image/png"]);
    }

    #[test]
    fn primary_selection_does_not_become_the_clipboard() {
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue::<State>();
        let qh = queue.handle();
        let backend = connection.backend().downgrade();
        let mut state = State::default();
        let device = ext_device::ExtDataControlDeviceV1::inert(backend.clone());
        let offer = ext_offer::ExtDataControlOfferV1::inert(backend.clone());
        state.offer(offer.id(), "image/png".into());
        <State as Dispatch<ext_device::ExtDataControlDeviceV1, ()>>::event(
            &mut state,
            &device,
            ext_device::Event::PrimarySelection { id: Some(offer.clone()) },
            &(),
            &connection,
            &qh,
        );
        assert!(state.selection.is_none());
        state.offer(offer.id(), "image/png".into());
        <State as Dispatch<ext_device::ExtDataControlDeviceV1, ()>>::event(
            &mut state,
            &device,
            ext_device::Event::Selection { id: Some(offer) },
            &(),
            &connection,
            &qh,
        );
        assert!(matches!(state.selection, Some(Some(Offer::Ext(_)))));
        state.selection = None;
        let device = wlr_device::ZwlrDataControlDeviceV1::inert(backend.clone());
        let offer = wlr_offer::ZwlrDataControlOfferV1::inert(backend);
        <State as Dispatch<wlr_device::ZwlrDataControlDeviceV1, ()>>::event(
            &mut state,
            &device,
            wlr_device::Event::PrimarySelection { id: Some(offer.clone()) },
            &(),
            &connection,
            &qh,
        );
        assert!(state.selection.is_none());
        state.offer(offer.id(), "image/png".into());
        <State as Dispatch<wlr_device::ZwlrDataControlDeviceV1, ()>>::event(
            &mut state,
            &device,
            wlr_device::Event::Selection { id: Some(offer) },
            &(),
            &connection,
            &qh,
        );
        assert!(matches!(state.selection, Some(Some(Offer::Wlr(_)))));
        assert!(Clipboard { connection, state }.has_png());
    }

    #[test]
    fn pipe_reads_are_bounded_and_include_the_exact_limit() {
        use std::io::Write;
        for length in [0, 32, MAX_BYTES, MAX_BYTES + 1] {
            let (reader, mut writer) = io::pipe().unwrap();
            let producer = std::thread::spawn(move || {
                let chunk = [42; 8192];
                let mut left = length;
                while left > 0 {
                    let count = left.min(chunk.len());
                    if writer.write_all(&chunk[..count]).is_err() {
                        break;
                    }
                    left -= count;
                }
            });
            let stop = AtomicBool::new(false);
            let result = read_pipe(reader, &Deadline::new(&stop));
            if length > MAX_BYTES {
                assert!(result.unwrap_err().to_string().contains("16 MiB"));
            } else {
                assert_eq!(result.unwrap(), vec![42; length]);
            }
            producer.join().unwrap();
        }
    }
}
