use std::io::Read;
use std::sync::atomic::Ordering;

use base64::Engine;
use crossbeam_channel::bounded;
use ratatui::buffer::Buffer;

use super::connection::send;
use super::transfer::{dropped_paths, explicit_paths};
use super::*;
use crate::attachments::Source;
use crate::client::clipboard_image::Diagnostic;
use crate::protocol;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::server::overlays;
use std::path::PathBuf;

struct Size(usize, usize);
impl alacritty_terminal::grid::Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}

#[test]
fn attachment_results_keep_their_kind_and_draw_it_locally_and_over_ssh() {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
    for remote in [false, true] {
        let (client, _peer) = controller(remote);
        for (kind, message, colour) in [
            (NoticeKind::Notice, "Files received.", NamedColor::White),
            (NoticeKind::Warning, "Focus changed: queued typing discarded.", NamedColor::BrightRed),
            (NoticeKind::Error, "Transfer interrupted.", NamedColor::BrightRed),
        ] {
            client.state.lock().unwrap().uncertain = Some("ab".into());
            client.message(ServerMsg::AttachmentResult {
                kind,
                token: "ab".into(),
                message: message.into(),
                pasted: kind != NoticeKind::Error,
            });
            assert_eq!(client.state.lock().unwrap().status_kind, kind);
            for (cols, rows) in [(80, 24), (124, 36)] {
                let mut bytes = Vec::new();
                client.draw_with(cols, rows, |out| bytes.extend_from_slice(out));
                let mut terminal = Term::new(Config::default(), &Size(cols as usize, rows as usize), VoidListener);
                let mut parser: Processor = Processor::new();
                parser.advance(&mut terminal, &bytes);
                let mut found = false;
                for row in 0..rows {
                    let line = Line(row as i32);
                    let text: String = (0..cols).map(|col| terminal.grid()[line][Column(col as usize)].c).collect();
                    if let Some(start) = text.find(message) {
                        found = true;
                        let start = text[..start].chars().count();
                        for col in start..start + message.chars().count() {
                            assert_eq!(terminal.grid()[line][Column(col)].fg, Color::Named(colour));
                        }
                    }
                }
                assert!(found, "missing result in attachment dialog: {message}");
            }
        }
    }
}

fn progress_client() -> (Arc<Controller>, Reader, Instant) {
    let (client, peer) = controller(true);
    let started = Instant::now();
    client.state.lock().unwrap().active = Some(Active {
        token: "ab".into(),
        cancelled: Arc::new(AtomicBool::new(false)),
        ack: bounded(2).0,
        started,
        last_draw: None,
        label: "capture.png".into(),
        progress: Some((0, 4 * 1048576)),
        failure: None,
    });
    (client, peer, started)
}

#[test]
fn progress_content_at_124_and_80_columns_in_each_theme() {
    for theme in [crate::theme::Theme::DEFAULT, crate::theme::Theme::ASCII] {
        for cols in [124, 80] {
            for percent in [0, 50, 100] {
                for label in ["capture.png", "2 files"] {
                    let (client, _peer, started) = progress_client();
                    let mut state = client.state.lock().unwrap();
                    let active = state.active.as_mut().unwrap();
                    active.label = label.into();
                    active.progress = Some((4 * 1048576 * percent / 100, 4 * 1048576));
                    state.status = Some(active.status(theme, started + PROGRESS_INTERVAL));
                    let mut view = state.view("SERVER").unwrap();
                    view.transfer = state.active.as_ref().unwrap().transfer(theme, started + PROGRESS_INTERVAL, false);
                    let (buf, buttons) = view.render(Rect::new(0, 0, cols, 35));
                    assert_eq!(buf.area.height, 9);
                    let bar_width = buf.area.width as usize - 6;
                    let filled = (percent as usize * bar_width + 50) / 100;
                    let (on, off) = if theme.ascii { ("#", "-") } else { ("▓", "░") };
                    let text = buf
                        .content
                        .chunks(buf.area.width as usize)
                        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                        .collect::<Vec<_>>()
                        .join("\n");
                    for expected in [
                        "Sending to SERVER".to_string(),
                        label.into(),
                        format!("{} / 4.0 MiB ({percent}%)", crate::text::human_size(4 * 1048576 * percent / 100)),
                        format!(
                            "{} {}{}",
                            if theme.ascii { "/" } else { "⠙" },
                            on.repeat(filled),
                            off.repeat(bar_width - filled)
                        ),
                        "Typing waits until it finishes.".into(),
                        "Ctrl+C Cancel".into(),
                    ] {
                        assert!(text.contains(&expected), "missing {expected:?} at {cols} columns:\n{text}");
                    }
                    assert_eq!(buttons.len(), 1);
                    if theme.ascii {
                        assert!(state.status.as_ref().unwrap().is_ascii());
                    }
                }
            }
        }
    }
}

#[test]
fn progress_timer_bounds_redraws_and_animates_without_acknowledgements() {
    let (client, peer, started) = progress_client();
    let mut frames = Vec::new();
    for millis in 0..=1000 {
        client.message(ServerMsg::AttachmentAck { token: "ab".into(), received: millis });
        assert!(client.draw_progress_with("ab", started + Duration::from_millis(millis), 80, 24, |bytes| {
            frames.push((millis, bytes.to_vec()));
        }));
    }
    assert_eq!(frames.iter().map(|(millis, _)| *millis).collect::<Vec<_>>(), [200, 400, 600, 800, 1000]);
    assert!(frames.windows(2).all(|pair| pair[0].1 != pair[1].1));
    assert!(peer.incoming.try_recv().is_err());
    let mut stalled = Vec::new();
    client.draw_progress_with("ab", started + Duration::from_millis(1200), 80, 24, |bytes| {
        stalled.extend_from_slice(bytes);
    });
    assert_ne!(frames.last().unwrap().1, stalled);
    assert!(String::from_utf8(stalled).unwrap().contains("0 B / 4.0 MiB (0%)"));
}

#[test]
fn incoming_output_reuses_progress_until_the_next_tick() {
    let (client, _peer, started) = progress_client();
    client.state.lock().unwrap().active.as_mut().unwrap().progress = None;
    for millis in [200, 400] {
        let mut displayed = Vec::new();
        client.draw_progress_with("ab", started + Duration::from_millis(millis), 80, 24, |out| {
            displayed.extend_from_slice(out);
        });
        client.progress("ab", millis * 1024, 4 * 1048576);
        client.draw_progress_with("ab", started + Duration::from_millis(millis + 199), 80, 24, |_| {
            panic!("progress must wait for its next tick");
        });
        client
            .output_with(b"\x1b[1;1Hpane output", 80, 24, |out| {
                let mut expected = b"\x1b[?2026h\x1b[1;1Hpane output".to_vec();
                expected.extend_from_slice(&displayed);
                expected.extend_from_slice(b"\x1b[?2026l");
                assert_eq!(out, expected, "incoming output must reuse the entire displayed snapshot");
                Ok(true)
            })
            .unwrap();
    }
    client.draw_progress_with("ab", started + Duration::from_millis(600), 80, 24, |out| {
        assert!(String::from_utf8_lossy(out).contains("400 KiB / 4.0 MiB (9%)"));
    });
}

#[test]
fn clearing_a_validation_error_repaints_uncovered_pane_content() {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    let (client, mut peer) = controller(false);
    let mut terminal = Term::new(Config::default(), &Size(80, 24), VoidListener);
    let mut parser: Processor = Processor::new();
    let background = (1..=24).map(|row| format!("\x1b[{row};1H{}", "p".repeat(80))).collect::<String>();
    parser.advance(&mut terminal, background.as_bytes());
    client.message(ServerMsg::AttachmentOffer {
        token: "ab".into(),
        target: "pane".into(),
        limits: Default::default(),
    });
    assert!(client.keys(b"\r"));
    client.draw_with(80, 24, |out| parser.advance(&mut terminal, out));
    let previous = client.state.lock().unwrap().drawn_area.unwrap();
    assert_eq!(previous.height, 10);
    assert_eq!(terminal.grid()[Line(previous.y as i32)][Column(previous.x as usize)].c, '╭');
    assert!(client.keys(b"\x15"));
    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
    client
        .output_with(background.as_bytes(), 80, 24, |out| {
            parser.advance(&mut terminal, out);
            Ok(true)
        })
        .unwrap();
    let current = client.state.lock().unwrap().drawn_area.unwrap();
    assert_eq!(current.height, 8);
    for row in previous.y..previous.bottom() {
        for col in previous.x..previous.right() {
            if !current.contains((col, row).into()) {
                assert_eq!(terminal.grid()[Line(row as i32)][Column(col as usize)].c, 'p');
            }
        }
    }
    assert_eq!(terminal.grid()[Line(current.y as i32)][Column(current.x as usize)].c, '╭');
    assert!(peer.incoming.try_recv().is_err());
}

#[test]
fn acknowledgements_do_not_wait_for_terminal_redraw() {
    let (client, _peer, started) = progress_client();
    client.draw_progress_with("ab", started + PROGRESS_INTERVAL, 80, 24, |_| {});
    let terminal = super::super::terminal();
    let (finished, done) = bounded(1);
    let worker = std::thread::spawn(move || {
        for received in 0..1000 {
            client.message(ServerMsg::AttachmentAck { token: "ab".into(), received });
        }
        finished.send(()).unwrap();
    });
    let completed = done.recv_timeout(Duration::from_secs(2)).is_ok();
    drop(terminal);
    worker.join().unwrap();
    assert!(completed, "acknowledgements must not draw a frame for every chunk");
}

#[test]
fn results_replace_progress_and_short_transfers_never_show_it() {
    for elapsed in [Duration::from_millis(199), PROGRESS_INTERVAL] {
        for pasted in [false, true] {
            let (client, mut peer, started) = progress_client();
            let mut frames = Vec::new();
            client.draw_progress_with("ab", started + elapsed, 80, 24, |bytes| frames.push(bytes.to_vec()));
            assert_eq!(frames.len(), usize::from(elapsed == PROGRESS_INTERVAL));
            client.progress("ab", 8 * 1048576, 8 * 1048576);
            let result = if pasted { "Files received." } else { "Transfer interrupted." };
            client.message(ServerMsg::AttachmentResult {
                token: "ab".into(),
                kind: if pasted { NoticeKind::Notice } else { NoticeKind::Error },
                message: result.into(),
                pasted,
            });
            if pasted {
                assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
            }
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            client.draw_with(80, 24, |bytes| {
                let text = String::from_utf8_lossy(bytes);
                assert!(text.contains(result));
                if pasted {
                    assert!(text.contains("8.0 MiB / 8.0 MiB (100%)"));
                }
                assert!(!text.contains("Sending to") && !text.contains("Ctrl+C Cancel"));
            });
            assert!(!client.draw_progress_with("ab", started + Duration::from_secs(1), 80, 24, |_| {
                panic!("progress must not repaint over a result");
            }));
        }
    }
}

#[test]
fn progress_preparation_sizes_and_filename_are_readable() {
    let (client, _peer, started) = progress_client();
    let mut state = client.state.lock().unwrap();
    let active = state.active.as_mut().unwrap();
    for theme in [crate::theme::Theme::DEFAULT, crate::theme::Theme::ASCII] {
        active.progress = None;
        assert!(active.status(theme, started).contains("Preparing attachment."));
        active.label = format!("capture\n\x1b{}", "界".repeat(80));
        for (received, total, expected) in
            [(0, 0, "0 B / 0 B (100%)"), (0, 3, "0 B / 3 B (0%)"), (512, 1024, "512 B / 1.0 KiB (50%)")]
        {
            active.progress = Some((received, total));
            let status = active.transfer(theme, started, false).unwrap().message(72);
            assert!(status.contains(expected));
            let label = status.lines().next().unwrap();
            assert!(!label.contains('\x1b'));
            assert!(crate::text::columns(label) <= 64);
        }
    }
}

#[test]
fn progress_draw_keeps_state_locked_until_output_finishes() {
    for (kind, message) in [(NoticeKind::Notice, "Files received."), (NoticeKind::Error, "Transfer interrupted.")] {
        let (client, _peer) = controller(false);
        client.state.lock().unwrap().status = Some("Sending".into());
        let frames = Mutex::new(Vec::new());
        let (snapshot_tx, snapshot_rx) = bounded(1);
        let (resume_tx, resume_rx) = bounded(1);
        let (locked_tx, locked_rx) = bounded(1);
        std::thread::scope(|scope| {
            let progress = scope.spawn(|| {
                client.draw_with(80, 24, |bytes| {
                    snapshot_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    frames.lock().unwrap().push(String::from_utf8(bytes.to_vec()).unwrap());
                });
            });
            snapshot_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let result = scope.spawn(|| {
                let locked = matches!(client.state.try_lock(), Err(std::sync::TryLockError::WouldBlock));
                locked_tx.send(locked).unwrap();
                {
                    let mut state = client.state.lock().unwrap();
                    state.status = Some(message.into());
                    state.status_kind = kind;
                }
                client.draw_with(80, 24, |bytes| {
                    frames.lock().unwrap().push(String::from_utf8(bytes.to_vec()).unwrap());
                });
            });
            let locked = locked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            resume_tx.send(()).unwrap();
            progress.join().unwrap();
            result.join().unwrap();
            assert!(locked, "a result must wait until the progress snapshot is written");
        });
        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].contains("Sending"));
        assert!(frames[1].contains(message));
    }
}

struct Wire(Sender<Vec<u8>>);
impl Write for Wire {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.send(bytes.to_vec()).map_err(std::io::Error::other)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct Reader {
    incoming: Receiver<Vec<u8>>,
    bytes: std::io::Cursor<Vec<u8>>,
}
impl Read for Reader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.bytes.position() == self.bytes.get_ref().len() as u64 {
            self.bytes = std::io::Cursor::new(
                self.incoming.recv_timeout(Duration::from_secs(2)).map_err(std::io::Error::other)?,
            );
        }
        self.bytes.read(out)
    }
}
fn wire() -> (Box<dyn Write + Send>, Reader) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (Box::new(Wire(tx)), Reader { incoming: rx, bytes: std::io::Cursor::new(Vec::new()) })
}
fn controller(remote: bool) -> (Arc<Controller>, Reader) {
    let (writer, peer) = wire();
    (Controller::new(Arc::new(Mutex::new(Some(writer))), remote), peer)
}
fn automatic_controller() -> (Arc<Controller>, Reader) {
    let target = super::super::Target::Remote {
        ssh: vec!["alice@preparing-test".into()],
        host: "alice@preparing-test".into(),
        session: "main".into(),
        label: "box".into(),
        pane: None,
        clipboard_native: false,
    };
    let (writer, peer) = wire();
    let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target, None);
    let (ssh, label) = client.destination.as_ref().unwrap();
    paste::remember(&paste::permission_path(), ssh, label).unwrap();
    assert!(paste::allowed(&paste::permission_path(), ssh));
    (client, peer)
}

fn read(peer: &mut Reader) -> ClientMsg {
    let (kind, data) = protocol::read_frame(peer).unwrap().unwrap();
    assert_eq!(kind, protocol::JSON);
    serde_json::from_slice(&data).unwrap()
}

fn read_without_repaint(peer: &mut Reader) -> ClientMsg {
    loop {
        match read(peer) {
            ClientMsg::Repaint | ClientMsg::Resize { .. } => {}
            message => return message,
        }
    }
}

fn dropped_file(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("kraal-pasted-{name}-{}", std::process::id()));
    std::fs::write(&path, b"abc").unwrap();
    path
}

fn prepare_drop(client: &Arc<Controller>, peer: &mut Reader, original: &[u8]) -> String {
    client.connected();
    client.input(&paste::bracket(original));
    assert!(matches!(read(peer), ClientMsg::PasteStart));
    let ClientMsg::AttachmentRequest { token, text, image: false, .. } = read(peer) else { panic!() };
    assert_eq!(text, original);
    client.message(ServerMsg::AttachmentPrepared {
        token: token.clone(),
        target: "main / Claude".into(),
        limits: Default::default(),
        broadcast: false,
    });
    token
}

#[test]
fn pending_keys_stay_before_pastes_and_pasted_control_v_never_probes() {
    for text in [b"".as_slice(), b"text\x16\x1b[118;5u"] {
        let (client, mut peer) = controller(true);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        let bytes = [b"\x1b".as_slice(), &paste::bracket(text), b"after"].concat();
        client.input_with_probe(&bytes, || panic!("paste data cannot inspect the clipboard"));
        let mut received = Vec::new();
        while received.len() < bytes.len() {
            let (kind, data) = protocol::read_frame(&mut peer).unwrap().unwrap();
            if kind == protocol::BYTES {
                received.extend(data);
            } else {
                assert!(matches!(serde_json::from_slice::<ClientMsg>(&data).unwrap(), ClientMsg::PasteStart));
            }
        }
        assert_eq!(received, bytes);
        assert!(!client.pending_keys());
    }
}

fn assert_clipboard_paste(peer: &mut Reader, expected_key: &[u8], expected_text: &[u8], paths: bool) {
    let ClientMsg::ClipboardPaste { pane, key, text, token } = read(peer) else {
        panic!("clipboard text must be bound to its agent pane");
    };
    assert_eq!(pane, 7);
    assert_eq!(key, expected_key);
    assert_eq!(text, expected_text);
    assert_eq!(token.is_some(), paths);
}

#[test]
fn cancelling_a_blocked_text_read_preserves_coalesced_and_later_typing() {
    for remote in [false, true] {
        for key in [b"\x03".as_slice(), b"\x1b[99;5u", b"\x1b[27;5;99~"] {
            for split in 0..=key.len() {
                let (client, mut peer) = controller(remote);
                client.connected();
                client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
                let (release, wait) = bounded(1);
                client.input_with_clipboard(
                    b"\x16discard",
                    || Ok(false),
                    move |cancelled, _| {
                        wait.recv_timeout(crate::testing::PATIENCE).unwrap();
                        assert!(cancelled.load(Ordering::SeqCst));
                        Ok(b"discard clipboard".to_vec())
                    },
                );
                client.input(&key[..split]);
                let tail = [&key[split..], b"following"].concat();
                if !client.input(&tail) {
                    client.forward(&tail);
                }
                assert!(client.state.lock().unwrap().clipboard_read.is_none());
                if !client.input(b"later") {
                    client.forward(b"later");
                }
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"following".to_vec()));
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"later".to_vec()));
                release.send(()).unwrap();
                client.wait_for_reads();
                assert!(peer.incoming.try_recv().is_err());
            }
        }
    }
}

#[test]
fn control_v_reads_agent_text_locally_and_remotely_and_leaves_shell_keys_alone() {
    for remote in [false, true] {
        for agent in [false, true] {
            for connected in [false, true] {
                for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
                    let (client, mut peer) = controller(remote);
                    if connected {
                        client.connected();
                    }
                    client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent });
                    let handled = client.input_with_clipboard(
                        key,
                        || Ok(false),
                        move |_, _| {
                            assert!(agent && connected);
                            Ok(b"hello\nworld".to_vec())
                        },
                    );
                    if !handled {
                        client.forward(key);
                    }
                    client.wait_for_reads();
                    if connected && agent {
                        assert_clipboard_paste(&mut peer, key, b"hello\nworld", false);
                    } else {
                        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, key.to_vec()));
                    }
                    assert!(peer.incoming.try_recv().is_err());
                }
            }
        }
    }
}

impl Controller {
    fn wait_for_reads(self: &Arc<Self>) {
        let workers = self.state.lock().unwrap().workers.clone();
        for done in workers {
            let _ = done.recv_timeout(crate::testing::PATIENCE);
        }
        self.input(&[]);
    }
}

#[test]
fn agent_text_clipboard_empty_overflow_and_exact_limit() {
    for remote in [false, true] {
        for bytes in [Vec::new(), b"12345".to_vec(), b"1234".to_vec()] {
            let (client, mut peer) = controller(remote);
            client.connected();
            client.set_clipboard_limit(4);
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
            let content = bytes.clone();
            client.input_with_clipboard(
                b"\x16",
                || Ok(false),
                move |_, limit| {
                    assert_eq!(limit, 4);
                    Ok(content.clone())
                },
            );
            client.wait_for_reads();
            if bytes.len() == 4 {
                assert_clipboard_paste(&mut peer, b"\x16", &bytes, false);
            } else {
                assert!(peer.incoming.try_recv().is_err());
                let shown = String::from_utf8(client.state.lock().unwrap().image_notices.render(160, 24)).unwrap();
                assert!(shown.contains(if bytes.is_empty() {
                    "The clipboard is empty"
                } else {
                    super::super::clipboard_image::TEXT_TOO_LARGE
                }));
            }
        }
    }
}

#[test]
fn text_clipboard_uses_the_normal_laptop_path_paste_flow() {
    let path = dropped_file("clipboard-path");
    for remote in [false, true] {
        let (client, mut peer) = controller(remote);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        let text = path.to_str().unwrap().as_bytes().to_vec();
        let content = text.clone();
        client.input_with_clipboard(b"\x16", || Ok(false), move |_, _| Ok(content.clone()));
        client.wait_for_reads();
        assert_clipboard_paste(&mut peer, b"\x16", &text, remote);
        assert_eq!(client.state.lock().unwrap().dropped.is_some(), remote);
        assert!(client.state.lock().unwrap().active.is_none());
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn pending_text_reads_cancel_on_control_c_focus_change_and_disconnect() {
    for remote in [false, true] {
        for action in 0..4 {
            let (client, mut peer) = controller(remote);
            client.connected();
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
            let (release, wait) = bounded(1);
            client.input_with_clipboard(
                b"\x16after",
                || Ok(false),
                move |cancelled, _| {
                    wait.recv_timeout(crate::testing::PATIENCE).unwrap();
                    assert_eq!(cancelled.load(Ordering::SeqCst), action != 0);
                    Ok(b"text".to_vec())
                },
            );
            match action {
                1 => {
                    client.input(b"\x03");
                }
                2 => client.message(ServerMsg::ClipboardFocus { pane: Some(8), agent: true }),
                3 => client.disconnect(),
                _ => {}
            }
            release.send(()).unwrap();
            client.wait_for_reads();
            if action == 0 {
                assert_clipboard_paste(&mut peer, b"\x16", b"text", false);
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"after".to_vec()));
            } else {
                assert!(peer.incoming.try_recv().is_err());
            }
        }
    }
}

#[test]
fn a_focus_lost_before_the_text_read_starts_neither_panics_nor_pastes() {
    // Cloning the reader runs between the offer's last focus check and the read.
    struct Leave(Arc<Controller>);
    impl Clone for Leave {
        fn clone(&self) -> Self {
            self.0.message(ServerMsg::ClipboardFocus { pane: None, agent: true });
            Leave(self.0.clone())
        }
    }
    for remote in [false, true] {
        let (client, mut peer) = controller(remote);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        let leave = Leave(client.clone());
        client.input_with_clipboard(
            b"\x16",
            || Ok(false),
            move |_, _| {
                let _leave = &leave;
                Ok(b"text".to_vec())
            },
        );
        client.wait_for_reads();
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"\x16".to_vec()));
        assert!(peer.incoming.try_recv().is_err());
    }
}

#[test]
fn a_cancelled_text_read_cannot_replace_a_later_reads_result() {
    for remote in [false, true] {
        let (client, mut peer) = controller(remote);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        let (release_old, wait_old) = bounded(1);
        client.input_with_clipboard(
            b"\x16",
            || Ok(false),
            move |_, _| {
                wait_old.recv_timeout(crate::testing::PATIENCE).unwrap();
                Ok(b"old".to_vec())
            },
        );
        let old_finished = client.state.lock().unwrap().workers[0].clone();
        client.disconnect();
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        let (release_new, wait_new) = bounded(1);
        client.input_with_clipboard(
            b"\x16",
            || Ok(false),
            move |_, _| {
                wait_new.recv_timeout(crate::testing::PATIENCE).unwrap();
                Ok(b"new".to_vec())
            },
        );
        release_old.send(()).unwrap();
        assert!(matches!(
            old_finished.recv_timeout(crate::testing::PATIENCE),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        ));
        assert!(client.state.lock().unwrap().clipboard_read.as_ref().unwrap().result.is_none());
        release_new.send(()).unwrap();
        client.wait_for_reads();
        assert_clipboard_paste(&mut peer, b"\x16", b"new", false);
        assert!(peer.incoming.try_recv().is_err());
    }
}

#[test]
fn a_terminal_paste_after_control_v_stays_after_the_clipboard_text() {
    for remote in [false, true] {
        for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
            let following = paste::bracket(b"terminal paste");
            for split in 0..=following.len() {
                let (client, mut peer) = controller(remote);
                client.connected();
                client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
                let (release, wait) = bounded(1);
                client.input_with_clipboard(
                    &[key, &following[..split]].concat(),
                    || Ok(false),
                    move |_, _| {
                        wait.recv_timeout(crate::testing::PATIENCE).unwrap();
                        Ok(b"clipboard text".to_vec())
                    },
                );
                client.input(&following[split..]);
                release.send(()).unwrap();
                client.wait_for_reads();
                assert_clipboard_paste(&mut peer, key, b"clipboard text", false);
                if remote {
                    assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
                }
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, following.clone()));
                assert!(peer.incoming.try_recv().is_err());
            }
        }
    }
}

#[test]
fn image_diagnostics_forward_keys_once_per_cause_and_survive_reconnection() {
    for key in [b"\x16".as_slice(), b"\x1b[118;5u", b"\x1b[118;133u", b"\x1b[118;5:1;118u"] {
        let (client, mut peer) = controller(true);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        for failure in 1..4 {
            for attempt in 0..2 {
                client.input_with_probe(key, move || match failure {
                    0 => Ok(false),
                    1 => Err(Diagnostic::MissingTool("wl-clipboard")),
                    2 => Err(Diagnostic::Failed("wl-paste: permission denied".into())),
                    _ => Err(Diagnostic::NoDisplay),
                });
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, key.to_vec()));
                let mut drawn = Vec::new();
                client.draw_with(80, 24, |bytes| drawn.extend_from_slice(bytes));
                assert_eq!(drawn.is_empty(), attempt != 0);
                let mut state = client.state.lock().unwrap();
                assert!(state.view("box").is_none(), "a diagnostic must not open a form");
                assert!(state.dropped.is_none());
                state.image_notices.clear();
            }
        }
        client.disconnect();
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        client.input_with_probe(key, || Err(Diagnostic::NoDisplay));
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, key.to_vec()));
        assert!(client.state.lock().unwrap().image_notices.render(80, 24).is_empty());
        client.input_with_probe(b"\r", || panic!("ordinary input must not probe"));
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![b'\r']));
    }
}

fn wait_for_probe(client: &Controller) {
    let done = client.state.lock().unwrap().image_probe.clone();
    if let Some(done) = done {
        done.recv_timeout(crate::testing::PATIENCE).unwrap();
    }
}

#[test]
fn a_stalled_non_agent_probe_does_not_hold_control_v_or_following_typing() {
    for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
        let (client, mut peer) = controller(true);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: false });
        let (entered, started) = bounded(1);
        let (resume, paused) = bounded(1);
        let input = Arc::clone(&client);
        let bytes = [key, b"same read"].concat();
        let thread = std::thread::spawn(move || {
            input.input_with_probe(&bytes, move || {
                entered.send(()).unwrap();
                paused.recv_timeout(crate::testing::PATIENCE).unwrap();
                Ok(true)
            });
            input.input(b"next read");
        });
        started.recv_timeout(crate::testing::PATIENCE).unwrap();
        let expected = [key, b"same readnext read"].concat();
        let mut received = Vec::new();
        while received.len() < expected.len() {
            let (kind, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(kind, protocol::BYTES);
            received.extend(bytes);
        }
        assert_eq!(received, expected);
        assert!(!client.state.lock().unwrap().image_notices.is_active());
        resume.send(()).unwrap();
        thread.join().unwrap();
        wait_for_probe(&client);
        assert!(client.state.lock().unwrap().image_notices.is_active());
    }
}

#[test]
fn copy_mode_control_v_neither_probes_nor_spends_the_unrecognized_notice() {
    let (client, mut peer) = controller(true);
    client.connected();
    client.message(ServerMsg::ClipboardFocus { pane: None, agent: true });
    client.input_with_probe(b"\x16", || panic!("copy mode must not inspect the clipboard"));
    assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![0x16]));
    assert!(!client.state.lock().unwrap().image_notices.is_active());
    client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: false });
    client.input_with_probe(b"\x16", || Ok(true));
    wait_for_probe(&client);
    assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![0x16]));
    assert!(client.state.lock().unwrap().image_notices.is_active());
}

#[test]
fn replacing_a_taller_notice_restores_its_whole_footprint_and_expiry_repaints() {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;
    let (client, mut peer) = controller(true);
    let mut terminal = Term::new(Config::default(), &Size(80, 24), VoidListener);
    let mut parser: Processor = Processor::new();
    let background = b"\x1b[2J\x1b[23;1Hpane content on row 23\x1b[24;1Hpane content on row 24";
    parser.advance(&mut terminal, background);
    client.image_notice(Diagnostic::NotAgent);
    client.draw_with(80, 24, |out| parser.advance(&mut terminal, out));
    let row = |terminal: &Term<VoidListener>, row| -> String {
        (0..80).map(|col| terminal.grid()[Line(row)][Column(col)].c).collect::<String>().trim_end().to_owned()
    };
    assert!(row(&terminal, 22).contains("not recognized as an agent"));
    client.image_notice(Diagnostic::Empty);
    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
    client
        .output_with(background, 80, 24, |out| {
            parser.advance(&mut terminal, out);
            Ok(true)
        })
        .unwrap();
    assert_eq!(row(&terminal, 22), "pane content on row 23");
    assert!(row(&terminal, 23).contains("The clipboard is empty"));
    client.expire_image_notice(Instant::now() + Duration::from_secs(5));
    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
    client
        .output_with(background, 80, 24, |out| {
            parser.advance(&mut terminal, out);
            Ok(true)
        })
        .unwrap();
    assert_eq!(row(&terminal, 22), "pane content on row 23");
    assert_eq!(row(&terminal, 23), "pane content on row 24");
    assert!(peer.incoming.try_recv().is_err());
}

#[test]
fn an_unrecognized_pane_only_explains_a_clipboard_that_advertises_an_image() {
    let (client, mut peer) = controller(true);
    client.connected();
    client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: false });
    for result in [Ok(false), Err(Diagnostic::MissingTool("wl-clipboard")), Err(Diagnostic::Failed("failed".into()))] {
        let result = Arc::new(Mutex::new(Some(result)));
        client.input_with_probe(b"\x16", move || result.lock().unwrap().take().unwrap());
        wait_for_probe(&client);
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![0x16]));
        assert!(client.state.lock().unwrap().image_notices.render(80, 24).is_empty());
    }
    for attempt in 0..2 {
        client.input_with_probe(b"\x16", || Ok(true));
        wait_for_probe(&client);
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, vec![0x16]));
        let mut state = client.state.lock().unwrap();
        assert_eq!(state.image_notices.render(124, 24).is_empty(), attempt != 0);
        assert!(state.dropped.is_none());
        state.image_notices.clear();
    }
    assert!(peer.incoming.try_recv().is_err());
}

#[test]
fn delayed_focus_updates_preserve_shortcut_and_coalesced_mode_keys_in_wire_order() {
    for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
        for coalesced in [false, true] {
            let (client, mut peer) = controller(true);
            client.connected();
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
            let prefix = b"\x02v";
            if coalesced {
                client.input_with_probe(&[prefix.as_slice(), key, b"l"].concat(), || Ok(true));
            } else {
                client.input(prefix);
                client.input_with_probe(key, || Ok(true));
                client.input(b"l");
            }
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, prefix.to_vec()));
            let ClientMsg::AttachmentRequest { token, text, image: true, pane: Some(7) } = read(&mut peer) else {
                panic!("the stale focus offers an image");
            };
            assert_eq!(text, key, "the server can replay the exact key before later input");
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"l".to_vec()));
            // The server has already entered copy mode, but its focus update arrives late.
            client.message(ServerMsg::ClipboardFocus { pane: None, agent: true });
            client.message(ServerMsg::AttachmentResult {
                token: token.clone(),
                pasted: true,
                message: String::new(),
                kind: NoticeKind::Notice,
            });
            assert!(
                matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentDismiss { token: sent } if sent == token)
            );
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            let state = client.state.lock().unwrap();
            assert!(state.dropped.is_none());
            assert!(state.view("box").is_none(), "normal input opens no failure dialog");
            assert!(state.workers.is_empty(), "no image was read");
            assert!(peer.incoming.try_recv().is_err());
        }
    }
}

#[test]
fn closing_a_cancelled_image_result_preserves_coalesced_typing() {
    for close in [b"\r".as_slice(), b"\x1b[13u", b"\x03"] {
        let (client, mut peer) = controller(true);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        client.input_with_probe(b"\x16", || Ok(true));
        let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
        client.message(ServerMsg::AttachmentPrepared {
            token: token.clone(),
            target: "Claude".into(),
            limits: Default::default(),
            broadcast: false,
        });
        client.input(b"\t\t\r");
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { token: sent } if sent == token));
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        client.message(ServerMsg::AttachmentResult {
            token: token.clone(),
            pasted: false,
            message: "Attachment stopped: cancelled".into(),
            kind: NoticeKind::Error,
        });
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        assert_eq!(client.state.lock().unwrap().view("box").unwrap().buttons, ["Close"]);
        client.input(&[close, b"cancelled"].concat());
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { token: sent } if sent == token));
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"cancelled".to_vec()));
        assert!(client.state.lock().unwrap().view("box").is_none());
        assert!(peer.incoming.try_recv().is_err());
    }
}

#[test]
fn control_v_question_keeps_the_key_local_and_cancel_reads_nothing() {
    for key in [b"\x16".as_slice(), b"\x1b[118;5u"] {
        for split in 1..=key.len() {
            for cancel in [b"\x1b".as_slice(), b"\t\t\r", b"\x03"] {
                let (client, mut peer) = controller(true);
                client.connected();
                client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
                client.input_with_probe(&key[..split], || Ok(true));
                if split < key.len() {
                    client.input_with_probe(&key[split..], || Ok(true));
                }
                let ClientMsg::AttachmentRequest { token, text, image: true, pane: Some(7) } = read(&mut peer) else {
                    panic!()
                };
                assert_eq!(text, key);
                client.message(ServerMsg::AttachmentPrepared {
                    token: token.clone(),
                    target: "main / Claude".into(),
                    limits: Default::default(),
                    broadcast: false,
                });
                {
                    let state = client.state.lock().unwrap();
                    let view = state.view("box").unwrap();
                    assert_eq!(view.title, "Send the clipboard image to box?");
                    assert_eq!(view.buttons, ["Send", "Always for box", "Cancel"]);
                    for width in [80, 124] {
                        assert_eq!(view.render(Rect::new(0, 0, width, 24)).1.len(), 3);
                    }
                    assert!(state.workers.is_empty());
                }
                assert!(peer.incoming.try_recv().is_err());
                client.input(cancel);
                client.input(&[]);
                assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { token: sent } if sent == token));
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                assert!(peer.incoming.try_recv().is_err());
                let state = client.state.lock().unwrap();
                assert!(!state.image_approved);
                assert!(state.workers.is_empty());
            }
        }
    }
}

#[test]
fn control_v_send_and_always_share_the_image_transport_without_sending_the_key() {
    if !crate::testing::in_own_process(
        "client::attachments::tests::control_v_send_and_always_share_the_image_transport_without_sending_the_key",
    ) {
        return;
    }
    let root = crate::paths::state_dir().join("shortcut-image-test");
    std::fs::create_dir_all(&root).unwrap();
    let log = root.join("reads");
    #[cfg(not(target_os = "macos"))]
    let (tool, script) = (
        "wl-paste",
        format!(
            "#!/bin/sh\n[ \"$WAYLAND_DISPLAY\" = test ] || exit 7\nif [ \"$1\" = --list-types ]; then printf 'image/png\\ntext/plain\\n'; exit; fi\necho read >> '{}'\nprintf '\\211PNG\\r\\n\\032\\n'\n",
            log.display()
        ),
    );
    #[cfg(target_os = "macos")]
    let (tool, script) = (
        "osascript",
        format!(
            "#!/bin/sh\nif [ \"$2\" = 'clipboard info' ]; then echo '«class PNGf», 8'; exit; fi\necho read >> '{}'\nfor arg; do file=$arg; done\nprintf '\\211PNG\\r\\n\\032\\n' > \"$file\"\n",
            log.display()
        ),
    );
    crate::testing::write_script(&root.join(tool), &script);
    // This isolated test has not started workers yet.
    unsafe {
        std::env::set_var("PATH", format!("{}:{}", root.display(), std::env::var("PATH").unwrap()));
        std::env::set_var("WAYLAND_DISPLAY", "test");
        std::env::remove_var("DISPLAY");
    }
    let make_client = || {
        let target = super::super::Target::Remote {
            ssh: vec!["alice@clipboard-test".into()],
            host: "alice@clipboard-test".into(),
            session: "main".into(),
            label: "box".into(),
            pane: None,
            clipboard_native: false,
        };
        let (writer, peer) = wire();
        let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target, None);
        client.connected();
        client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
        (client, peer)
    };
    for always in [false, true] {
        let (client, mut peer) = make_client();
        for attempt in 0..2 {
            assert!(client.input(b"\x16"));
            if attempt > 0 {
                assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
            }
            let ClientMsg::AttachmentRequest { token, image: true, .. } = read(&mut peer) else { panic!() };
            client.message(ServerMsg::AttachmentPrepared {
                token: token.clone(),
                target: "Claude".into(),
                limits: Default::default(),
                broadcast: false,
            });
            if attempt == 0 {
                assert!(client.state.lock().unwrap().active.is_none());
                client.input(if always { b"\t\r" } else { b"\r" });
            }
            assert!(client.state.lock().unwrap().active.is_some());
            let ClientMsg::AttachmentStart { files, .. } = read_without_repaint(&mut peer) else { panic!() };
            assert_eq!(files[0].name, "clipboard.png");
            client.message(ServerMsg::AttachmentAck { token: token.clone(), received: 0 });
            let ClientMsg::AttachmentChunk { data, .. } = read_without_repaint(&mut peer) else { panic!() };
            assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), b"\x89PNG\r\n\x1a\n");
            client.message(ServerMsg::AttachmentAck { token: token.clone(), received: 8 });
            assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentFinish { .. }));
            client.message(ServerMsg::AttachmentResult {
                token,
                pasted: true,
                kind: NoticeKind::Notice,
                message: "Image path pasted.".into(),
            });
            assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentDismiss { .. }));
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        }
        client.shutdown();
        let (client, mut peer) = make_client();
        client.input(b"\x16");
        let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
        client.message(ServerMsg::AttachmentPrepared {
            token,
            target: "Codex".into(),
            limits: Default::default(),
            broadcast: false,
        });
        assert_eq!(client.state.lock().unwrap().active.is_some(), always);
        client.shutdown();
    }
    assert!(std::fs::read_to_string(log).unwrap().lines().count() >= 4);
}

#[test]
fn bare_escape_cancels_initial_prompts_on_idle_input() {
    for remote in [false, true] {
        for image in [false, true] {
            let (client, mut peer) = controller(remote);
            client.connected();
            let token = "ab".to_string();
            client.message(if image {
                ServerMsg::ClipboardImageOffer {
                    token: token.clone(),
                    target: "pane".into(),
                    limits: Default::default(),
                }
            } else {
                ServerMsg::AttachmentOffer { token: token.clone(), target: "pane".into(), limits: Default::default() }
            });
            assert!(client.input(b"\x1b"));
            assert!(client.pending_keys());
            assert!(client.state.lock().unwrap().prompt.is_some());
            assert!(client.input(&[]));
            assert!(!client.pending_keys());
            assert!(client.state.lock().unwrap().prompt.is_none());
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { token: cancelled } if cancelled == token));
            assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            assert!(client.state.lock().unwrap().active.is_none());
        }
    }
}

#[test]
fn expired_initial_prompts_explain_close_and_reopen_without_retry() {
    for image in [false, true] {
        let (client, mut peer) = controller(true);
        let token = "ab".to_string();
        client.message(if image {
            ServerMsg::ClipboardImageOffer { token: token.clone(), target: "pane".into(), limits: Default::default() }
        } else {
            ServerMsg::AttachmentOffer { token: token.clone(), target: "pane".into(), limits: Default::default() }
        });
        let action = if image { "Attach clipboard image" } else { "Attach files" };
        client.message(ServerMsg::AttachmentResult {
            kind: NoticeKind::Error,
            token: token.clone(),
            message: format!("Attachment stopped: the reserved pane closed or the attachment timed out. No paths or queued typing pasted. Close this result, then use {action} to retry."),
            pasted: false,
        });
        {
            let state = client.state.lock().unwrap();
            assert!(state.prompt.is_none());
            assert!(state.dropped.is_none());
            let view = state.view("host").unwrap();
            assert_eq!(view.buttons, ["Close"]);
            assert!(view.message.contains(&format!("Close this result, then use {action} to retry.")));
            assert!(!view.message.contains("Choose Retry"));
        }
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        assert!(client.input(b"\r"));
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { token: dismissed } if dismissed == token));
        assert!(client.state.lock().unwrap().view("host").is_none());
    }
}

#[test]
fn long_filenames_scroll_without_hiding_destination_or_validation_error() {
    let root = std::env::temp_dir().join(format!("kraal-long-names-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let paths: Vec<_> = (0..32).map(|i| root.join(format!("{i}{}-end.txt", "x".repeat(190)))).collect();
    for path in &paths {
        std::fs::write(path, b"abc").unwrap();
    }
    for (cols, count) in [124, 80].into_iter().flat_map(|cols| [2, 32].map(|count| (cols, count))) {
        let (client, mut peer) = controller(true);
        let original = paths[..count].iter().map(|p| p.to_str().unwrap()).collect::<Vec<_>>().join(" ");
        let expected: Vec<_> = paths[..count]
            .iter()
            .flat_map(|p| {
                crate::text::fold(p.file_name().unwrap().to_str().unwrap(), if cols == 124 { 96 } else { 72 })
            })
            .collect();
        prepare_drop(&client, &mut peer, original.as_bytes());
        {
            let mut state = client.state.lock().unwrap();
            let dropped = state.dropped.as_mut().unwrap();
            dropped.broadcast = true;
            dropped.limits.file_bytes = 1;
        }
        client.input(b"\r");
        assert!(client.state.lock().unwrap().active.is_none());
        let screen = Rect::new(0, 0, cols, 23);
        let mut seen = std::collections::HashSet::new();
        let mut previous = None;
        loop {
            let state = client.state.lock().unwrap();
            let view = state.view("this machine").unwrap();
            let layout = view.layout(screen);
            let (buffer, buttons) = view.render(screen);
            let rows: Vec<String> = (buffer.area.y..buffer.area.bottom())
                .map(|y| (buffer.area.x..buffer.area.right()).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect();
            let shown = rows.join("\n");
            assert!(shown.contains("Their paths on this machine go to Claude."), "{shown}");
            assert!(shown.contains("Broadcasting is on. Paths go only to this pane."), "{shown}");
            assert!(shown.contains("file too large; use kraal cp or raise attachments.file_bytes"), "{shown}");
            assert_eq!(buttons.len(), 4);
            assert!(shown.contains("Send") && shown.contains("Cancel"));
            for line in &expected {
                if rows.iter().any(|row| row.contains(line)) {
                    seen.insert(line.clone());
                }
            }
            if count == 32 {
                assert!(layout.max_scroll > 0);
                assert!(shown.contains("PgUp/PgDn: filenames"));
            }
            if previous == Some(view.scroll) {
                assert!(expected.iter().all(|line| seen.contains(line)), "some filename rows were never readable");
                break;
            }
            previous = Some(view.scroll);
            drop(state);
            client.keys_with_screen(b"\x1b[6~", screen);
        }
        client.keys_with_screen(b"\x1b[5~", screen);
        if count == 32 {
            assert!(client.state.lock().unwrap().dropped.as_ref().unwrap().scroll < previous.unwrap());
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn image_offer_requires_local_enter_and_ordinary_paste_never_reads_the_clipboard() {
    for remote in [false, true] {
        let (client, mut peer) = controller(remote);
        client.connected();
        client.message(ServerMsg::ClipboardImageOffer {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
        });
        assert!(peer.incoming.try_recv().is_err());
        client.keys(b"\x16\x1b[200~\r\x1b[201~");
        assert!(client.state.lock().unwrap().active.is_none());
        assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().image);
        assert!(peer.incoming.try_recv().is_err());
        client.keys(b"\x03");
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { .. }));
        assert!(client.state.lock().unwrap().prompt.is_none());
    }
}

#[test]
fn image_buttons_cancel_with_keyboard_and_mouse_without_reading() {
    for mouse in [false, true] {
        let (client, mut peer) = controller(true);
        client.message(ServerMsg::ClipboardImageOffer {
            token: "ab".into(),
            target: "main / main › dev@anvil:~".into(),
            limits: Default::default(),
        });
        let view = client.state.lock().unwrap().view("fakehost").unwrap();
        assert_eq!(view.title, "Send the clipboard image to fakehost?");
        assert_eq!(view.buttons, ["Send", "Cancel"]);
        assert_eq!(view.selected, 1, "the server can open it while Enter is typed");
        assert!(view.message.starts_with("Its path on fakehost goes to dev@anvil:~."));
        assert!(peer.incoming.try_recv().is_err());
        if mouse {
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            let (_, buttons) = view.render(Rect::new(0, 0, cols, rows - 1));
            let cancel = buttons[1];
            client.keys(format!("\x1b[<0;{};{}M", cancel.x + 1, cancel.y + 1).as_bytes());
        } else {
            client.keys(b"\r");
        }
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { .. }));
        assert!(client.state.lock().unwrap().active.is_none());
        assert!(client.state.lock().unwrap().workers.is_empty());
    }
}

#[test]
fn image_offer_between_paste_fragments_requires_enter_outside_the_paste() {
    if !crate::testing::in_own_process(
        "client::attachments::tests::image_offer_between_paste_fragments_requires_enter_outside_the_paste",
    ) {
        return;
    }
    let root = crate::paths::state_dir().join("fragmented-image-test");
    std::fs::create_dir_all(&root).unwrap();
    for tool in ["wl-paste", "xclip", "osascript"] {
        crate::testing::write_script(&root.join(tool), "#!/bin/sh\nexit 1\n");
    }
    // This process has not started any workers yet. Never read the desktop clipboard.
    unsafe {
        std::env::set_var("PATH", &root);
        std::env::set_var("WAYLAND_DISPLAY", "test");
    }
    for text in [b"text".to_vec(), vec![b'x'; paste::LIMIT + 1]] {
        let bytes = paste::bracket(&[text, b"\r".to_vec()].concat());
        for split in [1, 5, 6, bytes.len() - 7, bytes.len() - 3] {
            let (client, _peer) = controller(true);
            client.connected();
            client.input(&bytes[..split]);
            client.message(ServerMsg::ClipboardImageOffer {
                token: "ab".into(),
                target: "pane".into(),
                limits: Default::default(),
            });
            client.input(&bytes[split..]);
            let state = client.state.lock().unwrap();
            assert!(state.active.is_none(), "paste authorized an image at split {split}");
            assert!(state.prompt.as_ref().is_some_and(|p| p.image), "prompt lost at split {split}");
            drop(state);
            client.input(b"\r");
            assert!(client.state.lock().unwrap().prompt.is_none());
            client.shutdown();
        }
    }
}

#[test]
fn images_read_once_and_use_acknowledged_chunks_even_for_local_sessions() {
    if !crate::testing::in_own_process(
        "client::attachments::tests::images_read_once_and_use_acknowledged_chunks_even_for_local_sessions",
    ) {
        return;
    }
    let root = crate::paths::state_dir().join("image-test");
    std::fs::create_dir_all(&root).unwrap();
    let log = root.join("reads");
    let temporary = root.join("tmp");
    std::fs::create_dir_all(&temporary).unwrap();
    #[cfg(not(target_os = "macos"))]
    let (tool, script) =
        ("wl-paste", format!("#!/bin/sh\necho read >> '{}'\nprintf '\\211PNG\\r\\n\\032\\n'\n", log.display()));
    #[cfg(target_os = "macos")]
    let (tool, script) = (
        "osascript",
        format!(
            "#!/bin/sh\necho read >> '{}'\nfor arg; do file=$arg; done\nprintf '\\211PNG\\r\\n\\032\\n' > \"$file\"\n",
            log.display()
        ),
    );
    crate::testing::write_script(&root.join(tool), &script);
    // This test runs alone, before starting clipboard or attachment workers.
    unsafe {
        std::env::set_var("PATH", format!("{}:{}", root.display(), std::env::var("PATH").unwrap()));
        std::env::set_var("TMPDIR", &temporary);
        std::env::set_var("WAYLAND_DISPLAY", "test");
        std::env::remove_var("DISPLAY");
    }
    for (remote, shortcut) in
        [false, true].into_iter().flat_map(|remote| [false, true].map(|shortcut| (remote, shortcut)))
    {
        let (client, mut peer) = controller(remote);
        if shortcut {
            client.connected();
            client.message(ServerMsg::ClipboardFocus { pane: Some(7), agent: true });
            client.input_with_probe(b"\x16", || Ok(true));
            let ClientMsg::AttachmentRequest { token, image: true, .. } = read(&mut peer) else { panic!() };
            assert!(client.state.lock().unwrap().active.is_none());
            client.message(ServerMsg::AttachmentPrepared {
                token,
                target: "pane".into(),
                limits: Default::default(),
                broadcast: false,
            });
            assert_eq!(client.state.lock().unwrap().active.is_some(), !remote);
        } else {
            client.message(ServerMsg::ClipboardImageOffer {
                token: "ab".into(),
                target: "pane".into(),
                limits: Default::default(),
            });
        }
        for attempt in 0..2 {
            let token = {
                let state = client.state.lock().unwrap();
                state
                    .prompt
                    .as_ref()
                    .map(|p| p.token.clone())
                    .or_else(|| state.dropped.as_ref().map(|d| d.token.clone()))
                    .unwrap()
            };
            if !shortcut || remote {
                // The offered image question opens on Cancel; a retry or Ctrl+V question on Send.
                client.keys(if attempt == 0 && !shortcut { b"\t\r" } else { b"\r" });
            }
            let ClientMsg::AttachmentStart { files, .. } = read_without_repaint(&mut peer) else {
                panic!("image did not use attachment transport")
            };
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].name, "clipboard.png");
            assert_eq!(files[0].size, 8);
            // Redraws share the writer with the upload worker, including between acknowledgements.
            send(&client.writer, &ClientMsg::Resize { cols: 90, rows: 30 }).unwrap();
            client.message(ServerMsg::AttachmentAck { token: token.clone(), received: 0 });
            let ClientMsg::AttachmentChunk { data, .. } = read_without_repaint(&mut peer) else { panic!() };
            assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), b"\x89PNG\r\n\x1a\n");
            send(&client.writer, &ClientMsg::Resize { cols: 91, rows: 30 }).unwrap();
            client.message(ServerMsg::AttachmentAck { token: token.clone(), received: 8 });
            assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentFinish { .. }));
            if attempt == 0 {
                client.message(ServerMsg::AttachmentResult {
                    kind: NoticeKind::Error,
                    token,
                    message: "Transfer interrupted. Choose Retry to try again.".into(),
                    pasted: false,
                });
                assert_eq!(client.state.lock().unwrap().view("server").unwrap().buttons, ["Retry", "Close"]);
                let reads = std::fs::read_to_string(&log).unwrap();
                client.keys(b"\t\r");
                assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                let ClientMsg::AttachmentRequest { token, image: true, .. } = read_without_repaint(&mut peer) else {
                    panic!()
                };
                client.message(ServerMsg::AttachmentPrepared {
                    token,
                    target: "pane".into(),
                    limits: Default::default(),
                    broadcast: false,
                });
                assert_eq!(client.state.lock().unwrap().active.is_some(), shortcut && !remote);
                if !shortcut || remote {
                    assert_eq!(std::fs::read_to_string(&log).unwrap(), reads, "retry must wait for local Send");
                    assert_eq!(
                        client.state.lock().unwrap().view("server").unwrap().buttons.len(),
                        if shortcut { 3 } else { 2 }
                    );
                }
            }
        }
        client.shutdown();
    }
    assert_eq!(std::fs::read_to_string(log).unwrap(), "read\n".repeat(8));
    assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
    let ready = root.join("ready");
    crate::testing::write_script(
        &root.join(tool),
        &format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n", ready.display()),
    );
    for stop in [None, Some(Controller::cancel as fn(&Controller)), Some(Controller::disconnect)] {
        let (client, _peer) = controller(true);
        client.message(ServerMsg::ClipboardImageOffer {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
        });
        client.keys(b"\t\r");
        let started = Instant::now();
        let pid: libc::pid_t = loop {
            if let Some(pid) = crate::testing::read_complete_lines(&ready).and_then(|text| text.trim().parse().ok()) {
                break pid;
            }
            assert!(started.elapsed() < crate::testing::PATIENCE);
            std::thread::sleep(Duration::from_millis(5));
        };
        if let Some(stop) = stop {
            stop(&client);
        }
        client.shutdown();
        assert!(client.state.lock().unwrap().active.is_none());
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0, "shutdown left an unfinished clipboard export");
        // SAFETY: signal zero only checks whether the helper still exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "shutdown left the clipboard helper running");
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        std::fs::remove_file(&ready).unwrap();
    }
}

#[test]
fn concurrent_shutdowns_wait_for_cancelled_workers_to_finish_cleanup() {
    for stop in [Controller::cancel as fn(&Controller), Controller::disconnect] {
        let (client, _peer) = controller(true);
        let (finished, completion) = bounded(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut state = client.state.lock().unwrap();
            state.workers.push(completion);
            state.active = Some(Active {
                token: "ab".into(),
                cancelled: cancelled.clone(),
                ack: bounded(1).0,
                started: Instant::now(),
                last_draw: None,
                label: String::new(),
                progress: None,
                failure: None,
            });
        }
        stop(&client);
        assert!(cancelled.load(Ordering::SeqCst));
        assert!(client.state.lock().unwrap().active.is_none());
        let (entered, entering) = bounded(2);
        let (done, completed) = bounded(2);
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let client = client.clone();
                let entered = entered.clone();
                let done = done.clone();
                std::thread::spawn(move || {
                    entered.send(()).unwrap();
                    client.shutdown();
                    done.send(()).unwrap();
                })
            })
            .collect();
        for _ in 0..2 {
            entering.recv_timeout(crate::testing::PATIENCE).unwrap();
        }
        // The worker still owns its completion sender while its destructors run.
        let early = completed.recv_timeout(Duration::from_millis(100));
        drop(finished);
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(matches!(early, Err(crossbeam_channel::RecvTimeoutError::Timeout)));
        assert_eq!(completed.len(), 2);
    }
}

#[test]
fn always_is_local_reused_on_next_attach_and_revoked_before_the_next_paste() {
    if !crate::testing::in_own_process(
        "client::attachments::tests::always_is_local_reused_on_next_attach_and_revoked_before_the_next_paste",
    ) {
        return;
    }
    let path = dropped_file("permission");
    let ssh = vec!["alice@box".into()];
    let target = super::super::Target::Remote {
        ssh: ssh.clone(),
        host: "alice@box".into(),
        session: "main".into(),
        label: "box".into(),
        pane: None,
        clipboard_native: false,
    };
    let (writer, mut peer) = wire();
    let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target, None);
    prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
    assert!(!paste::allowed(&paste::permission_path(), &ssh));
    client.keys(b"\t\r");
    assert!(paste::allowed(&paste::permission_path(), &ssh));
    assert!(client.state.lock().unwrap().active.is_some());
    client.disconnect();
    for (broadcast, revoke) in [(false, false), (true, false), (false, true)] {
        if revoke {
            std::fs::remove_file(paste::permission_path()).unwrap();
        }
        let (writer, mut peer) = wire();
        let client = Controller::for_target(Arc::new(Mutex::new(Some(writer))), &target, None);
        client.connected();
        // A server cannot create a local gesture, even with permission already granted.
        client.message(ServerMsg::AttachmentPrepared {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
            broadcast: false,
        });
        assert!(client.state.lock().unwrap().active.is_none());
        client.input(&paste::bracket(path.to_str().unwrap().as_bytes()));
        assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
        let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
        client.message(ServerMsg::AttachmentPrepared {
            token,
            target: "main / Claude".into(),
            limits: Default::default(),
            broadcast,
        });
        assert_eq!(client.state.lock().unwrap().active.is_some(), !broadcast && !revoke);
        client.disconnect();
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn paste_recognition_requires_remote_connection_and_preserves_ordinary_bytes() {
    let path = dropped_file("ordinary");
    for remote in [false, true] {
        let (client, peer) = controller(remote);
        let input = paste::bracket(path.to_str().unwrap().as_bytes());
        assert!(!client.input(&input));
        assert!(peer.incoming.try_recv().is_err());
        if !remote {
            client.connected();
            assert!(!client.input(&input));
        }
    }
    let (client, mut peer) = controller(true);
    client.connected();
    for original in [b"echo /some/path".as_slice(), b"/does/not/exist", b"\xff", b"", b"multiple\nlines"] {
        let input = paste::bracket(original);
        for byte in &input {
            assert!(client.input(&[*byte]));
        }
        if !original.is_empty() {
            assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
        }
        let (kind, data) = protocol::read_frame(&mut peer).unwrap().unwrap();
        assert_eq!(kind, protocol::BYTES);
        assert_eq!(data, input);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn original_text_is_kept_and_a_coalesced_tail_cannot_authorize_sending() {
    let path = dropped_file("text with spaces");
    let original = format!("  '{}'\r\n", path.display()).into_bytes();
    let (client, mut peer) = controller(true);
    client.connected();
    client.input(&[paste::bracket(&original), b"\r".to_vec()].concat());
    assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
    let ClientMsg::AttachmentRequest { token, text, image: false, .. } = read(&mut peer) else { panic!() };
    assert_eq!(text, original);
    let (kind, tail) = protocol::read_frame(&mut peer).unwrap().unwrap();
    assert_eq!(kind, protocol::BYTES);
    assert_eq!(tail, b"\r");
    client.message(ServerMsg::AttachmentPrepared {
        token: token.clone(),
        target: "main / Claude".into(),
        limits: Default::default(),
        broadcast: false,
    });
    assert!(client.state.lock().unwrap().active.is_none());
    assert!(peer.incoming.try_recv().is_err());
    client.keys(b"\t\t\r");
    assert!(
        matches!(read(&mut peer), ClientMsg::AttachmentText { token: sent, text } if sent == token && text == original)
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn first_choice_action_keeps_coalesced_trailing_input() {
    let path = dropped_file("choice-tail");
    for (keys, cancel) in [(b"\x03\r".as_slice(), true), (b"\rafter".as_slice(), false)] {
        let (client, mut peer) = controller(true);
        let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        client.input(keys);
        let mut started = false;
        let mut cancelled = false;
        let mut tail = Vec::new();
        // The action, redraw, and trailing bytes each produce a frame.
        for _ in 0..3 {
            let (kind, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
            if kind == protocol::BYTES {
                tail.extend(bytes);
            } else {
                match serde_json::from_slice::<ClientMsg>(&bytes).unwrap() {
                    ClientMsg::AttachmentStart { token: sent, .. } => {
                        assert_eq!(sent, token);
                        started = true;
                    }
                    ClientMsg::AttachmentCancel { token: sent } => {
                        assert_eq!(sent, token);
                        cancelled = true;
                    }
                    ClientMsg::Repaint => {}
                    other => panic!("unexpected message: {other:?}"),
                }
            }
        }
        assert_eq!(cancelled, cancel);
        assert_eq!(started, !cancel);
        assert_eq!(tail, if cancel { b"\r".as_slice() } else { b"after" });
        assert_eq!(client.state.lock().unwrap().active.is_some(), !cancel);
        client.disconnect();
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn cancellation_before_prepared_prevents_automatic_sending() {
    if !crate::testing::in_own_process(
        "client::attachments::tests::cancellation_before_prepared_prevents_automatic_sending",
    ) {
        return;
    }
    let path = dropped_file("cancel-preparing");
    for automatic in [false, true] {
        for coalesced in [false, true] {
            for cancel in [b"\x03".as_slice(), b"\x1b[99;5u"] {
                let (client, mut peer) = if automatic { automatic_controller() } else { controller(true) };
                client.connected();
                let paste = paste::bracket(path.to_str().unwrap().as_bytes());
                if coalesced {
                    client.input(&[paste.as_slice(), cancel, b"\r"].concat());
                } else {
                    client.input(&paste);
                    for byte in cancel {
                        client.input(std::slice::from_ref(byte));
                    }
                    client.input(b"\r");
                }
                assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
                let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
                loop {
                    let (kind, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
                    if kind == protocol::JSON {
                        assert!(matches!(serde_json::from_slice::<ClientMsg>(&bytes).unwrap(),
                        ClientMsg::AttachmentCancel { token: sent } if sent == token));
                        break;
                    }
                    assert!(!bytes.contains(&3));
                }
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"\r".to_vec()));
                client.message(ServerMsg::AttachmentPrepared {
                    token,
                    target: "main / Claude".into(),
                    limits: Default::default(),
                    broadcast: false,
                });
                let state = client.state.lock().unwrap();
                assert!(state.dropped.is_none());
                assert!(state.active.is_none());
                assert!(peer.incoming.try_recv().is_err());
            }
        }
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn consecutive_pastes_are_forwarded_while_preparing() {
    if !crate::testing::in_own_process("client::attachments::tests::consecutive_pastes_are_forwarded_while_preparing") {
        return;
    }
    let path = dropped_file("consecutive");
    for automatic in [false, true] {
        for coalesced in [false, true] {
            let (client, mut peer) = if automatic { automatic_controller() } else { controller(true) };
            client.connected();
            let first = paste::bracket(path.to_str().unwrap().as_bytes());
            let second = paste::bracket(b"Please review this file.\n");
            if coalesced {
                client.input(&[first, second.clone()].concat());
            } else {
                client.input(&first);
                client.input(&second);
            }
            assert!(matches!(read(&mut peer), ClientMsg::PasteStart));
            let ClientMsg::AttachmentRequest { token, .. } = read(&mut peer) else { panic!() };
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, second));
            assert!(peer.incoming.try_recv().is_err());
            client.message(ServerMsg::AttachmentPrepared {
                token,
                target: "main / Claude".into(),
                limits: Default::default(),
                broadcast: false,
            });
            assert_eq!(client.state.lock().unwrap().active.is_some(), automatic);
            client.disconnect();
        }
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn text_choice_preserves_separate_reads_before_its_result() {
    let path = dropped_file("text-tail");
    let (client, mut peer) = controller(true);
    let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
    client.input(b"\t\t\r");
    assert!(matches!(read(&mut peer), ClientMsg::AttachmentText { token: sent, .. } if sent == token));
    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
    for bytes in [b"later".as_slice(), b" typing", b"\r", &paste::bracket(b"instructions")] {
        client.input(bytes);
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, bytes.to_vec()));
    }
    assert!(client.state.lock().unwrap().uncertain.is_some());
    client.message(ServerMsg::AttachmentResult {
        kind: NoticeKind::Notice,
        token,
        message: "Text submitted.".into(),
        pasted: true,
    });
    assert!(client.state.lock().unwrap().dropped.is_none());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn immediate_cancellation_reply_keeps_the_local_upload_error() {
    struct ReplyOnCancel {
        client: std::sync::Weak<Controller>,
        bytes: Vec<u8>,
        reply: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    }
    impl Write for ReplyOnCancel {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            let mut input = std::io::Cursor::new(&self.bytes);
            if let Ok(Some((protocol::JSON, json))) = protocol::read_frame(&mut input)
                && let Ok(ClientMsg::AttachmentCancel { token }) = serde_json::from_slice(&json)
            {
                let client = self.client.upgrade().unwrap();
                // Deliver the reply before the cancellation write returns.
                {
                    let state = client.state.lock().unwrap();
                    if let Some(dropped) = &state.dropped {
                        dropped.error.as_ref().expect("error before cancel");
                    } else {
                        state.active.as_ref().unwrap().failure.as_ref().expect("image error before cancel");
                    }
                }
                let reply = client.clone();
                let handle = std::thread::spawn(move || {
                    reply.message(ServerMsg::AttachmentResult {
                        kind: NoticeKind::Error,
                        token,
                        message: "Transfer interrupted: cancelled.".into(),
                        pasted: false,
                    })
                });
                let started = Instant::now();
                while client.state.lock().unwrap().status.is_none() {
                    assert!(started.elapsed() < Duration::from_secs(2));
                    std::thread::sleep(Duration::from_millis(1));
                }
                // Redraw finishes after this write releases the writer.
                *self.reply.lock().unwrap() = Some(handle);
                self.bytes.clear();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for image in [false, true] {
        let path = dropped_file("immediate-error");
        let (client, mut peer) = controller(true);
        let token = if image {
            client.state.lock().unwrap().active = Some(Active {
                token: "ab".into(),
                cancelled: Arc::new(AtomicBool::new(false)),
                ack: bounded(2).0,
                started: Instant::now(),
                last_draw: None,
                label: String::new(),
                progress: None,
                failure: None,
            });
            "ab".to_string()
        } else {
            prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes())
        };
        let reply = Arc::new(Mutex::new(None));
        *client.writer.lock().unwrap() =
            Some(Box::new(ReplyOnCancel { client: Arc::downgrade(&client), bytes: Vec::new(), reply: reply.clone() }));
        std::fs::remove_file(&path).unwrap();
        let error = Source::open(path).err().unwrap();
        client.upload_failed(token, &AtomicBool::new(false), &error, image);
        reply.lock().unwrap().take().unwrap().join().unwrap();
        let state = client.state.lock().unwrap();
        let status = state.status.as_ref().unwrap();
        assert!(status.contains(&format!("{error:#}")));
        if image {
            assert!(status.contains("Image not sent:"));
            assert!(status.contains("Choose Retry"));
        } else {
            assert!(status.contains("Choose Retry"));
        }
    }
}

#[test]
fn failed_drop_retains_text_retry_and_close_and_success_does_not_offer_retry() {
    for action in [0, 1, 2] {
        let path = dropped_file(&format!("failure-{action}"));
        let original = format!("'{}'", path.display()).into_bytes();
        let (client, mut peer) = controller(true);
        let token = prepare_drop(&client, &mut peer, &original);
        client.message(ServerMsg::AttachmentResult {
            kind: NoticeKind::Error,
            token: token.clone(),
            message: "Transfer interrupted. No paths pasted.".into(),
            pasted: false,
        });
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        client.expire_success(Instant::now() + Duration::from_secs(60));
        assert_eq!(client.state.lock().unwrap().view("server").unwrap().buttons, ["Retry", "Paste as text", "Close"]);
        client.drop_action(action);
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { token: sent } if sent == token));
        if action == 0 {
            assert!(matches!(read(&mut peer), ClientMsg::AttachmentRequest { text, .. } if text == original));
        } else if action == 1 {
            let (kind, text) = protocol::read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(kind, protocol::BYTES);
            assert_eq!(text, paste::bracket(&original));
        }
        std::fs::remove_file(path).unwrap();
    }
    let path = dropped_file("success");
    let (client, mut peer) = controller(true);
    let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
    client.message(ServerMsg::AttachmentResult {
        kind: NoticeKind::Notice,
        token,
        message: "Files received.".into(),
        pasted: true,
    });
    assert!(client.state.lock().unwrap().dropped.is_none());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn directories_links_special_files_and_limits_are_refused_before_sending() {
    use std::os::unix::fs::symlink;
    let root = std::env::temp_dir().join(format!("kraal-drop-refusal-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("file");
    std::fs::write(&file, b"abc").unwrap();
    symlink(&file, root.join("link")).unwrap();
    let fifo = std::ffi::CString::new(root.join("fifo").to_str().unwrap()).unwrap();
    // SAFETY: a valid C path, created inside this test directory.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    for path in [&root, &root.join("link"), &root.join("fifo")] {
        let (client, mut peer) = controller(true);
        prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        assert!(client.state.lock().unwrap().dropped.as_ref().unwrap().error.as_ref().unwrap().contains("kraal cp"));
        client.keys(b"\r");
        assert!(client.state.lock().unwrap().active.is_none());
        assert!(peer.incoming.try_recv().is_err());
    }
    let (client, mut peer) = controller(true);
    prepare_drop(&client, &mut peer, file.to_str().unwrap().as_bytes());
    client.state.lock().unwrap().dropped.as_mut().unwrap().limits.file_bytes = 2;
    client.keys(b"\r");
    assert!(client.state.lock().unwrap().active.is_none());
    assert!(peer.incoming.try_recv().is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn terminal_documentation_shows_the_rendered_questions() {
    let root = std::env::temp_dir().join(format!("kraal-attachment-mockups-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let first = root.join("capture one.png");
    let second = root.join("notes.txt");
    std::fs::write(&first, vec![0; 300 * 1024]).unwrap();
    std::fs::write(&second, b"notes").unwrap();
    let (client, mut peer) = controller(true);
    prepare_drop(&client, &mut peer, format!("'{}' '{}'", first.display(), second.display()).as_bytes());
    let mut state = client.state.lock().unwrap();
    state.dropped.as_mut().unwrap().target = "dev@anvil:~".into();
    let view = state.view("SERVER").unwrap();
    for cols in [124, 80] {
        let (buf, _) = view.render(Rect::new(0, 0, cols, 23));
        let text = buf
            .content
            .chunks(buf.area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(include_str!("../../../docs/terminal.md").contains(&text), "missing {cols}-column mockup:\n{text}");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn question_boxes_restore_every_cell_at_124_and_80_columns() {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;
    struct Size(usize, usize);
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }
    let path = dropped_file("layout");
    for (cols, phase) in [124, 80]
        .into_iter()
        .flat_map(|cols| ["choice", "image", "progress", "failure", "success"].map(|phase| (cols, phase)))
    {
        let (client, mut peer) = controller(true);
        let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        {
            let mut state = client.state.lock().unwrap();
            match phase {
                "image" => state.dropped.as_mut().unwrap().image = true,
                "progress" => {
                    state.active = Some(Active {
                        token: token.clone(),
                        cancelled: Arc::new(AtomicBool::new(false)),
                        ack: bounded(2).0,
                        started: Instant::now(),
                        last_draw: None,
                        label: String::new(),
                        progress: None,
                        failure: None,
                    });
                    state.status = Some("0.1 / 0.3 MiB (33%)".into());
                }
                "failure" => {
                    state.dropped.as_mut().unwrap().result = true;
                    state.status = Some("Transfer interrupted. Choose Retry to try again.".into());
                    state.result_token = Some(token.clone());
                }
                "success" => {
                    state.dropped = None;
                    state.status = Some("Sent 1 file to fakehost".into());
                    state.success_until = Some(Instant::now());
                }
                _ => {}
            }
        }
        let screen = Rect::new(0, 0, cols, 24);
        let mut background = Buffer::empty(screen);
        for pos in screen.positions() {
            background[pos]
                .set_symbol(if pos.y == 23 {
                    "━"
                } else if pos.x == 20 {
                    "┃"
                } else {
                    "x"
                })
                .set_fg(ratatui::style::Color::Blue)
                .set_bg(ratatui::style::Color::DarkGray);
        }
        let mut encoder = crate::ansi::Encoder::new();
        let mut bytes = Vec::new();
        let cursor = Some(crate::ansi::Cursor { x: 25, y: 22, shape: crate::ansi::CursorShape::Bar, blinking: false });
        encoder.encode_with_images(&background, &[], cursor, &Default::default(), &[], &[], &mut bytes);
        let mut terminal = Term::new(Config::default(), &Size(cols as usize, 24), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut terminal, &bytes);
        let before = terminal.grid().clone();
        let view = client.state.lock().unwrap().view("fakehost").unwrap();
        let (buf, buttons) = view.render(Rect::new(0, 0, cols, 23));
        if phase == "choice" {
            assert_eq!(view.buttons, ["Send", "Always for fakehost", "Paste as text", "Cancel"]);
            assert_eq!(view.title, "Send 1 file to fakehost?");
            assert_eq!(buttons.len(), 4);
        }
        for (index, rect) in buttons.iter().enumerate() {
            assert!(buf.area.contains((rect.right() - 1, rect.y).into()));
            let style = overlays::button_style(index == 0, false);
            let cell = &buf[(rect.x, rect.y)];
            assert_eq!(Some(cell.fg), style.fg);
            assert_eq!(cell.bg, style.bg.unwrap_or_default());
            assert_eq!(cell.modifier, style.add_modifier);
        }
        let mode = *terminal.mode();
        parser.advance(&mut terminal, &crate::ansi::overlay(&buf));
        assert_eq!(terminal.grid().cursor.point, before.cursor.point);
        assert_eq!(*terminal.mode(), mode);
        for pos in screen.positions().filter(|pos| !buf.area.contains(*pos)) {
            assert_eq!(
                terminal.grid()[Line(pos.y as i32)][Column(pos.x as usize)],
                before[Line(pos.y as i32)][Column(pos.x as usize)]
            );
        }
        if phase == "success" {
            client.expire_success(Instant::now());
        } else {
            client.keys(if phase == "progress" { b"\x03" } else { b"\x1b" });
            if phase == "failure" {
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
            }
            client.keys(&[]);
            let message = read(&mut peer);
            if phase == "failure" {
                assert!(matches!(message, ClientMsg::AttachmentDismiss { token: sent } if sent == token));
            } else {
                assert!(matches!(message, ClientMsg::AttachmentCancel { token: sent } if sent == token));
            }
            if phase == "progress" {
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                client.message(ServerMsg::AttachmentResult {
                    kind: NoticeKind::Error,
                    token: token.clone(),
                    message: "Transfer cancelled.".into(),
                    pasted: false,
                });
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                client.keys(b"\x1b");
                client.keys(&[]);
                assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
            }
        }
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        assert!(client.state.lock().unwrap().view("fakehost").is_none());
        encoder.invalidate();
        bytes.clear();
        encoder.encode_with_images(&background, &[], cursor, &Default::default(), &[], &[], &mut bytes);
        parser.advance(&mut terminal, &bytes);
        for pos in screen.positions() {
            assert_eq!(
                terminal.grid()[Line(pos.y as i32)][Column(pos.x as usize)],
                before[Line(pos.y as i32)][Column(pos.x as usize)]
            );
        }
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn compact_boxes_and_every_button_at_124_and_80_columns() {
    if !crate::testing::in_own_process(
        "client::attachments::tests::compact_boxes_and_every_button_at_124_and_80_columns",
    ) {
        return;
    }
    let root = crate::testing::TempDir::new("attachment-buttons");
    let path = root.0.join("capture.png");
    std::fs::write(&path, b"image").unwrap();
    #[cfg(not(target_os = "macos"))]
    let (tool, script) = ("wl-paste", "#!/bin/sh\nprintf '\\211PNG\\r\\n\\032\\n'\n");
    #[cfg(target_os = "macos")]
    let (tool, script) =
        ("osascript", "#!/bin/sh\nfor arg; do file=$arg; done\nprintf '\\211PNG\\r\\n\\032\\n' > \"$file\"\n");
    crate::testing::write_script(&root.0.join(tool), script);
    // This isolated test has not started workers yet.
    unsafe {
        std::env::set_var("PATH", &root.0);
        std::env::set_var("WAYLAND_DISPLAY", "test");
        std::env::remove_var("DISPLAY");
    }
    for cols in [124, 80] {
        for (phase, height, labels, expected) in [
            ("question", 7, vec!["Send", "Always for this machine", "Paste as text", "Cancel"], "capture.png"),
            ("image", 6, vec!["Send", "Cancel"], "Its path on this machine goes to Claude."),
            (
                "shortcut",
                6,
                vec!["Send", "Always for this machine", "Cancel"],
                "Its path on this machine goes to Claude.",
            ),
            ("prompt", 8, vec!["Send", "Cancel"], "Enter absolute local paths, quoting spaces."),
            ("image prompt", 7, vec!["Send", "Cancel"], "Send reads the local clipboard once."),
            ("progress", 9, vec!["Cancel"], "0 B / 4.0 MiB (0%)"),
            ("failure", 6, vec!["Retry", "Paste as text", "Close"], "Transfer interrupted. Choose Retry to try again."),
            ("image failure", 6, vec!["Retry", "Close"], "Transfer interrupted. Choose Retry to try again."),
            ("result", 6, vec!["Close"], "No paths pasted. Close and try again."),
        ] {
            for index in 0..labels.len() {
                let (client, mut peer) = controller(true);
                let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
                {
                    let mut state = client.state.lock().unwrap();
                    if phase.contains("image") || phase == "shortcut" {
                        let dropped = state.dropped.as_mut().unwrap();
                        dropped.image = true;
                        dropped.shortcut = phase == "shortcut";
                        dropped.paths.clear();
                    }
                    if phase.contains("prompt") {
                        state.dropped = None;
                        state.prompt = Some(Prompt {
                            image: phase == "image prompt",
                            token: token.clone(),
                            target: "Claude".into(),
                            limits: Default::default(),
                            text: path.to_str().unwrap().into(),
                            cursor: 0,
                            error: None,
                            paste_failed: false,
                            parser: InputParser::new(),
                        });
                    } else if phase == "progress" {
                        state.active = Some(Active {
                            token: token.clone(),
                            cancelled: Arc::new(AtomicBool::new(false)),
                            ack: bounded(2).0,
                            started: Instant::now(),
                            last_draw: None,
                            label: "capture.png".into(),
                            progress: Some((0, 4 * 1048576)),
                            failure: None,
                        });
                        state.displayed =
                            state.active.as_ref().unwrap().transfer(crate::theme::current(), Instant::now(), false);
                        state.status = Some("Typing waits until it finishes.".into());
                    } else if phase.contains("failure") {
                        state.dropped.as_mut().unwrap().result = true;
                        state.status = Some(expected.into());
                        state.result_token = Some(token.clone());
                    } else if phase == "result" {
                        state.dropped = None;
                        state.status = Some(expected.into());
                        state.result_token = Some(token.clone());
                    }
                }
                let screen = Rect::new(0, 0, cols, 35);
                let view = client.state.lock().unwrap().view("this machine").unwrap();
                assert_eq!(view.buttons, labels);
                let (buf, buttons) = view.render(screen);
                assert_eq!(buf.area.width, if cols == 124 { 100 } else { 76 });
                assert_eq!(buf.area.height, height, "{phase} at {cols}");
                let rows = buffer_rows(&buf);
                assert!(rows.join("\n").contains(expected), "{phase}: {rows:?}");
                assert_eq!(buttons.len(), labels.len());
                assert_eq!(buttons[index].y, buf.area.bottom() - 3);
                assert!(rows[height as usize - 4].trim_matches(['│', ' ']).is_empty());
                assert!(rows[height as usize - 2].contains("Enter") || phase == "progress");
                let rect = buttons[index];
                assert_eq!(client.button_at(&client.state.lock().unwrap(), screen, rect.x, rect.y - 1), None);
                client.keys_with_screen(format!("\x1b[<0;{};{}M", rect.x + 1, rect.y + 1).as_bytes(), screen);
                match labels[index] {
                    "Send" | "Always for this machine" => {
                        assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentStart { .. }));
                        assert!(client.state.lock().unwrap().active.is_some());
                        client.shutdown();
                    }
                    "Retry" => {
                        assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                        assert!(matches!(read(&mut peer), ClientMsg::AttachmentRequest { .. }));
                    }
                    "Paste as text" if phase == "question" => {
                        assert!(matches!(read(&mut peer), ClientMsg::AttachmentText { .. }));
                    }
                    "Paste as text" => {
                        assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                        assert_eq!(
                            protocol::read_frame(&mut peer).unwrap().unwrap(),
                            (protocol::BYTES, paste::bracket(path.to_str().unwrap().as_bytes()))
                        );
                    }
                    "Close" => assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. })),
                    "Cancel" => assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { .. })),
                    _ => unreachable!(),
                }
            }
        }
    }
}

fn buffer_rows(buf: &Buffer) -> Vec<String> {
    buf.content.chunks(buf.area.width as usize).map(|row| row.iter().map(|cell| cell.symbol()).collect()).collect()
}

#[test]
fn final_transfer_keeps_its_content_and_closes_automatically() {
    let path = dropped_file("final");
    for cols in [124, 80] {
        for (image, count, label) in [(false, 2, "2 files"), (false, 1, "capture.png"), (true, 1, "clipboard.png")] {
            for focus_changed in [false, true] {
                let (client, mut peer) = controller(true);
                let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
                {
                    let mut state = client.state.lock().unwrap();
                    let dropped = state.dropped.as_mut().unwrap();
                    dropped.image = image;
                    dropped.paths = vec![path.clone(); count];
                    state.active = Some(Active {
                        token: token.clone(),
                        cancelled: Arc::new(AtomicBool::new(false)),
                        ack: bounded(2).0,
                        started: Instant::now(),
                        last_draw: None,
                        label: label.into(),
                        progress: Some((57 * 1048576, 57 * 1048576)),
                        failure: None,
                    });
                }
                let before = Instant::now();
                client.message(ServerMsg::AttachmentResult {
                    token,
                    pasted: true,
                    kind: if focus_changed { NoticeKind::Warning } else { NoticeKind::Notice },
                    message: if focus_changed { "Files received. Focus changed." } else { "Files received." }.into(),
                });
                assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
                assert!(matches!(read(&mut peer), ClientMsg::Repaint));
                let (mut view, until) = {
                    let state = client.state.lock().unwrap();
                    (state.view("SERVER").unwrap(), state.success_until.unwrap())
                };
                // Both themes use the same final size and geometry.
                for ascii in [false, true] {
                    let transfer = view.transfer.as_mut().unwrap();
                    transfer.ascii = ascii;
                    transfer.marker = if ascii { 'v' } else { '✓' };
                    let (buf, buttons) = view.render(Rect::new(0, 0, cols, 35));
                    assert_eq!(buf.area.height, 6);
                    assert!(buttons.is_empty());
                    let rows = buffer_rows(&buf);
                    assert!(rows[0].contains("Sent to this machine"));
                    assert!(rows[1].contains(label));
                    assert!(rows[2].contains("57 MiB / 57 MiB (100%)"));
                    assert!(rows[3].contains(&format!(
                        "{} {}",
                        if ascii { 'v' } else { '✓' },
                        if ascii { "#" } else { "▓" }.repeat(buf.area.width as usize - 6)
                    )));
                    assert!(rows[4].contains(if focus_changed {
                        "Focus changed. Select Claude before typing again."
                    } else if image || count == 1 {
                        "Path pasted into Claude."
                    } else {
                        "Paths pasted into Claude."
                    }));
                    assert!(!rows.join("\n").contains("Typing waits"));
                }
                assert!(until >= before + SUCCESS_DURATION);
                assert!(until <= Instant::now() + SUCCESS_DURATION);
                client.input(b"typing\x03");
                assert_eq!(
                    protocol::read_frame(&mut peer).unwrap().unwrap(),
                    (protocol::BYTES, b"typing\x03".to_vec())
                );
                client.expire_success(until);
            }
        }
    }
    let (client, mut peer, _) = progress_client();
    client.message(ServerMsg::AttachmentResult {
        token: "ab".into(),
        pasted: true,
        kind: NoticeKind::Notice,
        message: "Files received.".into(),
    });
    assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { .. }));
    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
    assert!(client.state.lock().unwrap().view("SERVER").is_none());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn success_expires_without_swallowing_typing_or_control_c() {
    let path = dropped_file("notice");
    for image in [false, true] {
        let (client, mut peer) = controller(true);
        let token = prepare_drop(&client, &mut peer, path.to_str().unwrap().as_bytes());
        client.state.lock().unwrap().dropped.as_mut().unwrap().image = image;
        client.message(ServerMsg::AttachmentResult {
            kind: NoticeKind::Notice,
            token: token.clone(),
            message: "Files received. Paths submitted to the reserved pane. No Enter added.".into(),
            pasted: true,
        });
        assert!(matches!(read(&mut peer), ClientMsg::AttachmentDismiss { token: sent } if sent == token));
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        let until = {
            let state = client.state.lock().unwrap();
            assert_eq!(state.status.as_deref(), Some("Sent to this machine\nPath pasted into Claude."));
            assert!(state.view("this machine").unwrap().buttons.is_empty());
            state.success_until.unwrap()
        };
        for keys in [b"typing".as_slice(), b"\x03", b"\x1b[99;5u"] {
            client.input(keys);
            assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, keys.to_vec()));
        }
        client.expire_success(until - Duration::from_millis(1));
        assert!(client.state.lock().unwrap().status.is_some());
        client.expire_success(until);
        assert!(client.state.lock().unwrap().view("this machine").is_none());
        assert!(matches!(read(&mut peer), ClientMsg::Repaint));
        client.input(b"\x03");
        assert_eq!(protocol::read_frame(&mut peer).unwrap().unwrap(), (protocol::BYTES, b"\x03".to_vec()));
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn cancellation_parses_encoded_fragmented_and_coalesced_keys_in_every_state() {
    for input in [b"\x03".as_slice(), b"\x1b[99;5u", b"before\x03after", b"before\x1b[99;5uafter"] {
        for fragmented in [false, true] {
            for phase in ["prompt", "active", "result"] {
                let (client, mut peer) = controller(true);
                let cancelled = Arc::new(AtomicBool::new(false));
                if phase == "prompt" {
                    client.message(ServerMsg::AttachmentOffer {
                        token: "ab".into(),
                        target: "pane".into(),
                        limits: Default::default(),
                    });
                } else {
                    let mut state = client.state.lock().unwrap();
                    if phase == "active" {
                        state.active = Some(Active {
                            token: "ab".into(),
                            cancelled: cancelled.clone(),
                            ack: bounded(2).0,
                            started: Instant::now(),
                            last_draw: None,
                            label: String::new(),
                            progress: None,
                            failure: None,
                        });
                    } else {
                        state.status = Some("Failed".into());
                        state.result_token = Some("ab".into());
                    }
                }
                if fragmented {
                    for byte in input {
                        client.keys(std::slice::from_ref(byte));
                    }
                } else {
                    assert!(client.keys(input));
                }
                let message = read(&mut peer);
                if phase == "result" {
                    assert!(matches!(message, ClientMsg::AttachmentDismiss { token } if token == "ab"));
                    assert!(client.state.lock().unwrap().status.is_none());
                } else {
                    assert!(matches!(message, ClientMsg::AttachmentCancel { token } if token == "ab"));
                    assert!(client.state.lock().unwrap().active.is_none());
                    assert!(client.state.lock().unwrap().prompt.is_none());
                    if phase == "active" {
                        assert!(cancelled.load(Ordering::SeqCst));
                    }
                }
            }
        }
    }
}

#[test]
fn unbracketed_overflow_cannot_submit_an_independently_valid_prefix() {
    let (client, peer) = controller(true);
    let path = std::env::temp_dir().join(format!("kraal-attachment-overflow-{}", std::process::id()));
    std::fs::write(&path, b"abc").unwrap();
    client.message(ServerMsg::AttachmentOffer {
        token: "ab".into(),
        target: "pane".into(),
        limits: Default::default(),
    });
    let prefix = format!("{}{}", path.display(), " ".repeat(4096 - path.to_str().unwrap().len()));
    client.keys(format!("{prefix}{}", path.display()).as_bytes());
    {
        let state = client.state.lock().unwrap();
        let prompt = state.prompt.as_ref().unwrap();
        assert_eq!(prompt.text, prefix);
        assert_eq!(explicit_paths(&prompt.text).unwrap(), vec![path.clone()]);
        assert!(prompt.error.is_some());
    }
    client.keys(b"\x1b[D\x1b[C\x1b[200~\x1b[201~\r");
    assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().error.is_some());
    assert!(client.state.lock().unwrap().active.is_none());
    assert!(peer.incoming.try_recv().is_err());
    client.keys(b"\x15");
    assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().error.is_none());
    assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().text.is_empty());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn progress_and_cancellation_are_visible_without_the_first_acknowledgement() {
    for acknowledge_start in [false, true] {
        let (client, mut peer) = controller(true);
        let path = std::env::temp_dir().join(format!("kraal-attachment-progress-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        client.message(ServerMsg::AttachmentOffer {
            token: "ab".into(),
            target: "pane".into(),
            limits: Default::default(),
        });
        client.keys(format!("{}\r", path.display()).as_bytes());
        assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentStart { .. }));
        if acknowledge_start {
            client.message(ServerMsg::AttachmentAck { token: "ab".into(), received: 0 });
            assert!(matches!(read_without_repaint(&mut peer), ClientMsg::AttachmentChunk { .. }));
        }
        let started = Instant::now();
        loop {
            if let Some(view) = client.state.lock().unwrap().view("SERVER") {
                let (buf, _) = view.render(Rect::new(0, 0, 80, 23));
                let status = buffer_rows(&buf).join("\n");
                assert!(status.contains(path.file_name().unwrap().to_str().unwrap()));
                assert!(status.contains("0 B / 3 B (0%)"));
                assert!(status.contains("Typing waits"));
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(client.keys(b"\x1b[99;5u"));
        client.disconnect();
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn a_server_offer_never_opens_or_sends_a_file_without_local_confirmation() {
    let (client, mut peer) = controller(true);
    client.state.lock().unwrap().prompt = Some(Prompt {
        image: false,
        token: "ab".into(),
        target: "pane".into(),
        limits: Default::default(),
        text: "/must/not/be/opened".into(),
        cursor: 0,
        error: None,
        paste_failed: false,
        parser: InputParser::new(),
    });
    assert!(peer.incoming.try_recv().is_err());
    assert!(client.keys(b"\x03"));
    assert!(matches!(read(&mut peer), ClientMsg::AttachmentCancel { token } if token == "ab"));
    assert!(!client.keys(b"normal input"));
    assert!(explicit_paths("file:///tmp/nope").is_err());
    assert!(explicit_paths("/tmp/nope\\").is_err());
}

#[test]
fn a_single_chunk_window_waits_for_each_ack_and_keyboard_can_share_the_writer() {
    let (client, mut peer) = controller(true);
    let path = std::env::temp_dir().join(format!("kraal-attachment-window-{}", std::process::id()));
    std::fs::write(&path, vec![b'x'; attachments::CHUNK + 7]).unwrap();
    let prompt = Prompt {
        image: false,
        token: "ab".into(),
        target: "pane".into(),
        limits: Default::default(),
        text: path.to_str().unwrap().into(),
        cursor: 0,
        error: None,
        paste_failed: false,
        parser: InputParser::new(),
    };
    let (acks, replies) = bounded(2);
    let worker = client.clone();
    let handle = std::thread::spawn(move || worker.upload(&prompt, &replies, &AtomicBool::new(false)));
    assert!(matches!(read(&mut peer), ClientMsg::AttachmentStart { .. }));
    // No ack means no file bytes. The keyboard still obtains the writer immediately.
    send(&client.writer, &ClientMsg::Resize { cols: 90, rows: 30 }).unwrap();
    assert!(matches!(read(&mut peer), ClientMsg::Resize { cols: 90, .. }));
    acks.send(0).unwrap();
    let ClientMsg::AttachmentChunk { offset, data, .. } = read(&mut peer) else { panic!() };
    assert_eq!(offset, 0);
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap().len(), attachments::CHUNK);
    send(&client.writer, &ClientMsg::Resize { cols: 91, rows: 30 }).unwrap();
    assert!(matches!(read(&mut peer), ClientMsg::Resize { cols: 91, .. }));
    acks.send(attachments::CHUNK as u64).unwrap();
    let ClientMsg::AttachmentChunk { offset, data, .. } = read(&mut peer) else { panic!() };
    assert_eq!(offset, attachments::CHUNK as u64);
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), b"xxxxxxx");
    acks.send(attachments::CHUNK as u64 + 7).unwrap();
    assert!(matches!(read(&mut peer), ClientMsg::AttachmentFinish { .. }));
    handle.join().unwrap().unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn cancelled_upload_cannot_write_to_a_replacement_connection() {
    let (client, mut peer) = controller(true);
    let path = std::env::temp_dir().join(format!("kraal-attachment-cancel-{}", std::process::id()));
    std::fs::write(&path, b"abc").unwrap();
    let prompt = Prompt {
        image: false,
        token: "ab".into(),
        target: "pane".into(),
        limits: Default::default(),
        text: path.to_str().unwrap().into(),
        cursor: 0,
        error: None,
        paste_failed: false,
        parser: InputParser::new(),
    };
    let (acks, replies) = bounded(2);
    let cancelled = Arc::new(AtomicBool::new(false));
    client.state.lock().unwrap().active = Some(Active {
        token: "ab".into(),
        cancelled: cancelled.clone(),
        ack: acks,
        started: Instant::now(),
        last_draw: None,
        label: String::new(),
        progress: None,
        failure: None,
    });
    let worker = client.clone();
    let handle = std::thread::spawn(move || worker.upload(&prompt, &replies, &cancelled));
    assert!(matches!(read(&mut peer), ClientMsg::AttachmentStart { .. }));
    client.disconnect();
    let (replacement, mut new_peer) = wire();
    *client.writer.lock().unwrap() = Some(replacement);
    assert!(handle.join().unwrap().is_err());
    assert!(new_peer.incoming.try_recv().is_err());
    client.reconnect();
    assert!(matches!(read(&mut new_peer), ClientMsg::AttachmentStatus { token } if token == "ab"));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn local_upload_submits_original_paths_without_file_chunks() {
    let (client, mut peer) = controller(false);
    let path = std::env::temp_dir().join(format!("kraal-attachment-local-{}", std::process::id()));
    std::fs::write(&path, b"abc").unwrap();
    let prompt = Prompt {
        image: false,
        token: "ab".into(),
        target: "pane".into(),
        limits: Default::default(),
        text: path.to_str().unwrap().into(),
        cursor: 0,
        error: None,
        paste_failed: false,
        parser: InputParser::new(),
    };
    let (_acks, replies) = bounded(2);
    client.upload(&prompt, &replies, &AtomicBool::new(false)).unwrap();
    assert!(
        matches!(read(&mut peer), ClientMsg::AttachmentLocal { paths, .. } if paths == vec![path.to_str().unwrap()])
    );
    std::fs::remove_file(path).unwrap();
}
#[test]
fn the_form_uses_shared_editing_and_keeps_typing_after_enter_in_order() {
    let (client, mut peer) = controller(true);
    let path = std::env::temp_dir().join(format!("kraal-attachment-form-{}", std::process::id()));
    std::fs::write(&path, b"abc").unwrap();
    client.state.lock().unwrap().prompt = Some(Prompt {
        image: false,
        token: "ab".into(),
        target: "pane".into(),
        limits: Default::default(),
        text: String::new(),
        cursor: 0,
        error: None,
        paste_failed: false,
        parser: InputParser::new(),
    });
    assert!(client.keys(b"wrong"));
    assert!(client.keys(b"\x15"));
    assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().text.is_empty());
    assert!(client.keys(b"\x1b[200~/bad\npath\x1b[201~"));
    assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().error.is_some());
    assert!(client.keys(b"\r"));
    assert!(client.state.lock().unwrap().active.is_none());
    client.keys(b"\x15");
    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
    client.keys(b"\x1b[200~");
    client.keys(&vec![b'x'; 128 * 1024]);
    client.keys(format!(" {}\x1b[201~\r", path.display()).as_bytes());
    assert!(client.state.lock().unwrap().prompt.as_ref().unwrap().error.is_some());
    assert!(client.state.lock().unwrap().active.is_none());
    client.keys(b"\x15");
    assert!(matches!(read(&mut peer), ClientMsg::Repaint));
    client.keys(format!("{}\rlater", path.display()).as_bytes());
    let mut started = false;
    let mut typed = false;
    for _ in 0..3 {
        let (kind, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
        if kind == protocol::BYTES {
            assert_eq!(bytes, b"later");
            typed = true;
        } else if matches!(serde_json::from_slice::<ClientMsg>(&bytes).unwrap(), ClientMsg::AttachmentStart { .. }) {
            started = true;
        }
    }
    assert!(started && typed);
    client.disconnect();
    std::fs::remove_file(path).unwrap();
}
#[test]
fn a_recovered_result_cannot_cancel_a_new_batch() {
    let (client, _peer) = controller(true);
    let cancelled = Arc::new(AtomicBool::new(false));
    let (ack, _replies) = bounded(2);
    {
        let mut state = client.state.lock().unwrap();
        state.uncertain = Some("ab".into());
        state.active = Some(Active {
            token: "cd".into(),
            cancelled: cancelled.clone(),
            ack,
            started: Instant::now(),
            last_draw: None,
            label: String::new(),
            progress: None,
            failure: None,
        });
    }
    client.message(ServerMsg::AttachmentResult {
        kind: NoticeKind::Notice,
        pasted: true,
        token: "ab".into(),
        message: "Files received.".into(),
    });
    let state = client.state.lock().unwrap();
    assert_eq!(state.active.as_ref().unwrap().token, "cd");
    assert!(!cancelled.load(Ordering::SeqCst));
    assert!(state.uncertain.is_none());
    assert!(state.status.is_none());
}

#[test]
fn dropped_paths_handle_local_uris_quotes_and_unique_raw_names_without_shell_expansion() {
    let t = crate::testing::TempDir::new("dropped-paths");
    let names =
        ["notes client.txt", "single'quote", "double\"quote", "back\\slash", "$literal", "brackets[1]", "café.png"];
    for name in names {
        let path = t.0.join(name);
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(dropped_paths(&crate::shell::quote(path.to_str().unwrap())), Some(vec![path.clone()]));
    }
    let path = t.0.join("notes client.txt");
    assert_eq!(dropped_paths(path.to_str().unwrap()), Some(vec![path.clone()]));
    let uri = format!("file://{}", path.display().to_string().replace(' ', "%20"));
    assert_eq!(dropped_paths(&uri), Some(vec![path.clone()]));
    assert_eq!(dropped_paths(&format!("  '{uri}'\r\n'{}'\t", path.display())), Some(vec![path.clone(), path.clone()]));
    for text in [
        format!("cat {}", path.display()),
        format!("{}\\", path.display()),
        format!("'{}", path.display()),
        format!("file://remote{}", path.display()),
        format!("{uri}%"),
        format!("{uri}%00"),
        format!("{uri}?query"),
        format!("{uri}#fragment"),
        format!("{}/$literal", t.0.display()),
        format!("{}/brackets[1]", t.0.display()),
        "/$(touch /tmp/no)".into(),
        "relative".into(),
        String::new(),
    ] {
        assert_eq!(dropped_paths(&text), None, "{text}");
    }
    let a = t.0.join("a");
    let b = t.0.join("b");
    std::fs::write(&a, b"a").unwrap();
    std::fs::write(&b, b"b").unwrap();
    let text = format!("{} {}", a.display(), b.display());
    let raw = PathBuf::from(&text);
    std::fs::create_dir_all(raw.parent().unwrap()).unwrap();
    std::fs::write(&raw, b"ambiguous").unwrap();
    assert_eq!(dropped_paths(&text), None);
    assert_eq!(dropped_paths(&format!("'{}' '{}'", a.display(), b.display())), Some(vec![a, b]));
    assert_eq!(dropped_paths(&format!("'{text}'")), Some(vec![raw]));
    let c = t.0.join("c");
    std::fs::write(&c, b"c").unwrap();
    assert_eq!(dropped_paths(&format!("{text} {}", c.display())), None);
    assert_eq!(dropped_paths(&format!("'{}' {text}", c.display())), None);
    let backslash = t.0.join("back\\slash");
    assert_eq!(dropped_paths(&format!("\"{}\"", backslash.display())), Some(vec![backslash]));
}

#[test]
fn dropped_paths_parsing() {
    let dir = std::env::temp_dir().join(format!("kraal drop {}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("it's here.txt");
    std::fs::write(&f, "x").unwrap();
    let escaped = f.display().to_string().replace(' ', "\\ ").replace('\'', "\\'");
    assert_eq!(dropped_paths(&escaped), Some(vec![f.clone()]));
    let quoted = format!("'{}' {}", dir.display(), escaped);
    assert_eq!(dropped_paths(&quoted), Some(vec![dir.clone(), f.clone()]));
    assert_eq!(dropped_paths("echo hello"), None);
    assert_eq!(dropped_paths("relative/path"), None);
    assert_eq!(
        dropped_paths(&format!("file://{}", dir.display().to_string().replace(' ', "\\ "))),
        Some(vec![dir.clone()])
    );
    let _ = std::fs::remove_dir_all(dir);
}
