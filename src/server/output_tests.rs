use super::*;
use crate::pane::PaneSpec;
use crate::server::ui::{Prompt, PromptAction};

fn pane(server: &mut Server) -> PaneId {
    let batch = crate::pane::batch();
    let id = server.spawn_pane(None, PaneSpec { command: vec!["true".into()], ..Default::default() }).unwrap();
    drop(batch);
    id
}

fn publish(server: &Server, id: PaneId, data: &[u8]) {
    server.panes[&id].term().unwrap().publish_output(id, Some(data), &server.tx);
}

fn input(server: &Server, client: ClientId, bytes: &[u8]) {
    let tx = server.tx.clone();
    let bytes = bytes.to_vec();
    std::thread::spawn(move || tx.send(Event::ClientFrame(client, crate::protocol::BYTES, bytes)).unwrap())
        .join()
        .unwrap();
}

#[test]
fn input_interrupts_many_ready_resized_panes_before_they_are_drained() {
    for (bytes, expected) in [(b"k".as_slice(), "k"), (b"\x1b[200~paste\x1b[201~", "paste"), (b"\x1b[<65;2;2M", "")] {
        let (tx, rx) = queue::channel(1);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        server.clients.get_mut(&client).unwrap().mode =
            Mode::Prompt(Prompt::new("Name", "", PromptAction::RenamePane(0)));
        let ids: Vec<_> = (0..16).map(|_| pane(&mut server)).collect();
        for &id in &ids {
            server.panes.get_mut(&id).unwrap().resize(80, 20);
            publish(&server, id, &vec![b'x'; TYPING_SLICE * 4]);
            server.redraws.insert(id, Instant::now() + Duration::from_secs(60));
        }
        assert!(server.event_phase(&rx, rx.wait(Duration::ZERO)));
        assert_eq!(server.ready.len(), ids.len(), "overflow recovered every pane");
        server.next_render = None;
        let before = server.clients[&client].last_input;
        // Input arrives after the event phase, while the resize pass is still owed.
        input(&server, client, bytes);
        server.pump_output(Duration::from_secs(60), &rx);
        let read = ids.iter().filter(|id| !server.panes[id].live_text().trim().is_empty()).count();
        assert_eq!(read, 1, "only the round-robin fairness slice precedes input");
        assert_eq!(server.unparsed.len(), 1);
        assert!(server.event_phase(&rx, rx.wait(Duration::ZERO)));
        assert!(server.clients[&client].last_input > before, "the real client handler ran");
        let Mode::Prompt(prompt) = &server.clients[&client].mode else { panic!("prompt") };
        assert_eq!(prompt.value, expected);
        // Leaving the interrupted work queued must not lose the rest of any pane's output.
        server.redraws.clear();
        for _ in 0..ids.len() * 10 {
            server.next_render = Some(Instant::now());
            server.pump_output(Duration::ZERO, &rx);
        }
        assert!(server.unparsed.is_empty());
        assert!(ids.iter().all(|id| !server.panes[id].has_output()));
        for id in ids {
            assert_eq!(server.panes[&id].text(Some(100)).matches('x').count(), TYPING_SLICE * 4);
        }
    }
}

#[test]
fn an_echo_without_a_new_notification_interrupts_a_background_batch() {
    let (tx, rx) = queue::channel(64);
    let mut server = tests::server(tx);
    let (client, _peer) = tests::client(&mut server);
    let focus = pane(&mut server);
    server.workspaces[0].tabs[0].root = Some(Node::Leaf(focus));
    server.workspaces[0].tabs[0].focus = Some(focus);
    publish(&server, focus, b"");
    let ids: Vec<_> = (0..15).map(|_| pane(&mut server)).collect();
    for &id in &ids {
        publish(&server, id, &vec![b'x'; TYPING_SLICE * 4]);
        server.redraws.insert(id, Instant::now() + Duration::from_secs(60));
    }
    input(&server, client, b"k");
    assert!(server.event_phase(&rx, rx.wait(Duration::ZERO)));
    assert!(server.echo.contains_key(&focus));
    let mut data = Vec::new();
    assert!(server.take_output(ids[0], &mut data, true));
    // The focused pane was already polled as a stream, so its echo needs no new event.
    server.panes[&focus].take_output_into(&mut Vec::new(), true);
    publish(&server, focus, b"key-echo");
    assert!(rx.is_empty());
    assert!(!server.parse_output(ids[0], &data, &rx, false));
    assert_eq!(server.unparsed[&ids[0]].len(), data.len() - TYPING_SLICE);
    assert!(ids[1..].iter().all(|id| server.panes[id].live_text().trim().is_empty()));
    server.next_render = None;
    server.pump_output(Duration::from_secs(60), &rx);
    assert!(server.panes[&focus].live_text().contains("key-echo"));
    assert!(!server.echo.contains_key(&focus));
    assert!(server.next_render.is_some_and(|at| at <= Instant::now()));
    assert!(ids[1..].iter().all(|id| server.panes[id].live_text().trim().is_empty()));
    assert_eq!(server.unparsed[&ids[0]].len(), data.len() - TYPING_SLICE);
    assert!(ids[1..].iter().all(|id| server.redraws.contains_key(id)));
    server.render_clients();
    let frame: String = server.clients[&client].frame.content().iter().map(|cell| cell.symbol()).collect();
    assert!(frame.contains("key-echo"), "the echo is drawn before the pending redraw batches");
    // Another due frame must still leave the other panes their bounded fairness turn.
    server.ready.retain(|id| *id != focus);
    let next = server.ready[0];
    let remaining = if next == ids[0] { data.len() - TYPING_SLICE } else { data.len() };
    server.render_now();
    server.pump_output(Duration::from_secs(60), &rx);
    assert!(!server.turn_skipped);
    assert_eq!(server.unparsed[&next].len(), remaining - TYPING_SLICE);
    assert!(ids[1..].iter().filter(|id| **id != next).all(|id| server.panes[id].live_text().trim().is_empty()));
}

#[test]
fn silent_or_expired_echoes_do_not_cut_output_into_slices() {
    for expired in [false, true] {
        let (tx, rx) = queue::channel(4);
        let mut server = tests::server(tx);
        let focus = pane(&mut server);
        let other = pane(&mut server);
        let until = if expired {
            publish(&server, focus, b"late");
            Instant::now() - Duration::from_secs(1)
        } else {
            Instant::now() + Duration::from_secs(60)
        };
        assert!(server.event_phase(&rx, rx.wait(Duration::ZERO)));
        server.echo.insert(focus, until);
        assert!(server.parse_output(other, &vec![b'x'; PARSE_SLICE * 2], &rx, false));
        assert!(server.unparsed.is_empty());
    }
}
