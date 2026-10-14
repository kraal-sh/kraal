//! Local consent and directory listing for a remote session's file manager.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    pane: PaneId,
    side: Side,
    request: u64,
    path: String,
}

struct Prompt {
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
    ack: Option<(PaneId, Side, u64, crossbeam_channel::Sender<()>)>,
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
        self.send(&ClientMsg::LaptopListed { pane: r.pane, side: r.side, request: r.request, result });
    }

    pub fn request(self: &Arc<Self>, pane: PaneId, side: Side, request: u64, path: String) {
        let request = Request { pane, side, request, path };
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
            let scope = crate::config::Config::load()
                .map(|config| config.laptop_files.scope)
                .map_err(|e| {
                    format!("Cannot read laptop configuration: {e}. Fix config.toml on the laptop and press r")
                })
                .and_then(|scope| {
                    if self.destination.is_none() {
                        Err("Laptop files require attach --host".into())
                    } else if scope == Scope::Off {
                        Err("Laptop files are off: change laptop_files.scope on the laptop and press r".into())
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
            if state.allowed || paste::allowed(&self.permissions, ssh) {
                state.busy = true;
                let generation = state.generation;
                drop(state);
                self.start(request, scope, generation);
            } else {
                state.prompt = Some(Prompt { request, scope, selected: 0 });
                drop(state);
                self.draw();
            }
            return;
        }
    }

    fn start(self: &Arc<Self>, request: Request, scope: Scope, generation: u64) {
        let this = self.clone();
        let retry = request.clone();
        let spawned = crate::threads::named("laptop-list").spawn(move || {
            let result =
                dirs::home_dir().ok_or_else(|| "Cannot find the laptop home directory".to_string()).and_then(|home| {
                    crate::laptop::list(&request.path, &home, scope, cfg!(target_os = "macos"))
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
            match event {
                InputEvent::Key(key) => match key.code {
                    KeyCode::Tab | KeyCode::Right => prompt.selected = (prompt.selected + 1) % 3,
                    KeyCode::BackTab | KeyCode::Left => prompt.selected = (prompt.selected + 2) % 3,
                    KeyCode::Enter => {
                        answer = Some(prompt.selected);
                        break;
                    }
                    KeyCode::Esc => {
                        answer = Some(2);
                        break;
                    }
                    KeyCode::Char('c') if key.mods.ctrl => {
                        answer = Some(2);
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
            let mut error = None;
            if answer == 0 {
                state.allowed = true;
            }
            if answer == 1
                && let Some((ssh, label)) = &self.destination
            {
                error = paste::remember(&self.permissions, ssh, label).err().map(|e| {
                    format!("Cannot save laptop permission: {e}. Fix laptop-files.toml on the laptop and press r")
                });
            }
            state.busy = answer != 2 && error.is_none();
            let generation = state.generation;
            drop(state);
            self.send(&ClientMsg::Repaint);
            if answer == 2 {
                let host = self.identity.as_ref().map_or("laptop", |i| i.host.as_str());
                self.reply(prompt.request, Err(format!("{host} refused: nothing was listed. Press r to ask again")));
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
    }

    fn view(&self, prompt: &Prompt) -> View {
        let host = self.identity.as_ref().map_or("laptop", |i| i.host.as_str());
        let server = self.destination.as_ref().map_or("server", |(_, label)| label.as_str());
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
                "{server}'s file manager asks to list {scope}.\nCopy, move and delete are not yet available.\nAllow lasts until this kraal attach ends. Always is kept in ~/.config/kraal/laptop-files.toml."
            ),
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
    use std::time::Duration;

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
    fn scope_off_and_invalid_config_refuse_even_a_remembered_server() {
        if !crate::testing::in_own_process(
            "client::laptop::tests::scope_off_and_invalid_config_refuse_even_a_remembered_server",
        ) {
            return;
        }
        let dir = TempDir::new("laptop-off");
        let (client, mut peer) = controller(dir.0.join("laptop-files.toml"));
        let (ssh, name) = client.destination.as_ref().unwrap();
        paste::remember(&client.permissions, ssh, name).unwrap();
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
    fn consent_names_come_from_the_client_and_describe_only_listing() {
        let dir = TempDir::new("laptop-dialog");
        let (client, _) = controller(dir.0.join("laptop-files.toml"));
        let prompt = Prompt {
            request: Request { pane: 1, side: Side::Left, request: 1, path: "server-controlled".into() },
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
                assert!(text.contains("not yet available"));
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
            Request { pane: 1, side: Side::Left, request: 1, path: "~".into() },
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
            request: Request { pane: 1, side: Side::Left, request: 1, path: "~".into() },
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
                Request { pane: 1, side: Side::Left, request: 1, path: "~".into() },
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
            worker.finish(0, Request { pane: 1, side: Side::Left, request: 1, path: "~".into() }, result)
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
            worker.finish(0, Request { pane: 1, side: Side::Left, request: 1, path: "~".into() }, large_listing())
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
            worker.finish(0, Request { pane: 1, side: Side::Left, request: 1, path: "~".into() }, large_listing())
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
            pane.reconnect_laptop("abc123", 8);
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
        paste::remember(&client.permissions, ssh, name).unwrap();
        client.state.lock().unwrap().busy = true;
        client.request(2, Side::Left, 2, "~".into());
        std::fs::remove_file(&client.permissions).unwrap();
        client.finish(
            0,
            Request { pane: 1, side: Side::Left, request: 1, path: "~".into() },
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
            Request { pane: 3, side: Side::Left, request: 3, path: "~".into() },
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
}
