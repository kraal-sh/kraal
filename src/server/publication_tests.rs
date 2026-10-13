use super::*;
use crate::pane::PaneSpec;

fn pane(server: &mut Server) -> PaneId {
    let batch = crate::pane::batch();
    let spec = PaneSpec { command: vec!["true".into()], ..Default::default() };
    let id = server.spawn_pane(None, spec).unwrap();
    drop(batch);
    id
}

fn publish(server: &Server, id: PaneId, data: Option<&[u8]>) {
    server.panes[&id].term().unwrap().publish_output(id, data, &server.tx);
}

fn turn(server: &mut Server, rx: &EventReceiver) {
    assert!(server.event_phase(rx, rx.wait(Duration::ZERO)));
    server.next_render = None;
    server.pump_output(Duration::from_millis(20), rx);
}

#[test]
fn rejected_final_output_is_parsed_once_without_a_tick_or_later_producer() {
    let (tx, rx) = queue::channel(1);
    let mut server = tests::server(tx);
    let id = pane(&mut server);
    server.tx.wake();
    publish(&server, id, Some(b"final-burst\r\n"));
    let last_tick = server.last_tick;
    turn(&mut server, &rx);
    assert_eq!(server.panes[&id].live_text().matches("final-burst").count(), 1);
    turn(&mut server, &rx);
    assert_eq!(server.panes[&id].live_text().matches("final-burst").count(), 1);
    assert_eq!(server.last_tick, last_tick);
    assert!(matches!(rx.wait(Duration::ZERO), Wait::Timeout));
}

#[test]
fn rejected_eof_finishes_an_exit_without_bytes_or_a_timer() {
    let (tx, rx) = queue::channel(1);
    let mut server = tests::server(tx);
    let id = pane(&mut server);
    server.handle_and_follow(Event::PaneExited(id, None, Some(7)));
    assert!(server.exiting.contains_key(&id));
    server.tx.wake();
    publish(&server, id, None);
    let last_tick = server.last_tick;
    turn(&mut server, &rx);
    assert!(server.exiting.is_empty());
    assert_eq!(server.panes[&id].exit_code, Some(Some(7)));
    assert_eq!(server.last_tick, last_tick);
}

#[test]
fn overflow_and_ordinary_readiness_do_not_duplicate_unparsed_or_watched_output() {
    let (tx, rx) = queue::channel(1);
    let mut server = tests::server(tx);
    server.env.scrollback = 1000;
    let id = pane(&mut server);
    server.panes.get_mut(&id).unwrap().resize(80, 20);
    server.tx.wake();
    let data: String = (1..=400).map(|i| format!("{i:04} abcdefghijklmnopqrstuv\r\n")).collect();
    publish(&server, id, Some(data.as_bytes()));
    assert!(server.event_phase(&rx, rx.wait(Duration::ZERO)));
    let mut read = Vec::new();
    assert!(server.take_output(id, &mut read, false));
    assert!(!server.parse_output(id, &read, &rx, true));
    assert!(server.unparsed.contains_key(&id));
    server.handle_and_follow(Event::PaneReady(id));
    server.reconcile_panes();
    assert_eq!(server.ready.iter().filter(|p| **p == id).count(), 1);
    turn(&mut server, &rx);
    assert!(!server.unparsed.contains_key(&id));
    let text = server.panes[&id].text(Some(1000));
    let numbers: Vec<u32> = text.lines().filter_map(|l| l.split_whitespace().next()?.parse().ok()).collect();
    assert_eq!(numbers, (1..=400).collect::<Vec<_>>());
    // A watched pane already belongs to ready_set; overflow does not add a second entry.
    server.ready.clear();
    server.watched = vec![id];
    publish(&server, id, Some(b"unique-tail"));
    server.tx.wake();
    server.tx.pane_ready(id);
    assert!(server.event_phase(&rx, rx.wait(Duration::ZERO)));
    assert!(server.ready.is_empty());
    assert_eq!(server.watched, [id]);
    server.ready.extend(server.watched.drain(..));
    turn(&mut server, &rx);
    assert_eq!(server.panes[&id].live_text().matches("unique-tail").count(), 1);
}

#[test]
fn reconciliation_uses_current_panes_after_removal_or_restart() {
    let (tx, rx) = queue::channel(1);
    let mut server = tests::server(tx);
    let removed = pane(&mut server);
    let restarted = pane(&mut server);
    server.tx.wake();
    publish(&server, removed, Some(b"removed"));
    publish(&server, restarted, Some(b"old-process"));
    server.panes.remove(&removed);
    let batch = crate::pane::batch();
    server.panes.get_mut(&restarted).unwrap().respawn(&server.env, &server.tx).unwrap();
    drop(batch);
    publish(&server, restarted, Some(b"new-process"));
    turn(&mut server, &rx);
    assert!(!server.ready_set.contains(&removed));
    let screen = server.panes[&restarted].live_text();
    assert!(!screen.contains("old-process"));
    assert_eq!(screen.matches("new-process").count(), 1);
}

#[test]
fn a_selected_token_is_kept_across_handlers_and_selected_events_keep_fifo() {
    let (tx, rx) = queue::channel(1);
    let mut server = tests::server(tx);
    let id = pane(&mut server);
    server.tx.wake();
    publish(&server, id, Some(b"after-handlers"));
    assert!(matches!(rx.try_recv(), Ok(Event::Wake)));
    let first = rx.wait(Duration::ZERO);
    assert!(matches!(first, Wait::Overflow));
    let worker = server.tx.clone();
    std::thread::spawn(move || worker.send(Event::Warning("first".into())).unwrap()).join().unwrap();
    let event = rx.wait(Duration::ZERO);
    let worker = server.tx.clone();
    std::thread::spawn(move || worker.send(Event::Warning("second".into())).unwrap()).join().unwrap();
    assert!(server.event_phase(&rx, event));
    assert_eq!(server.warnings, ["first", "second"]);
    assert!(server.ready.is_empty(), "the consumed token is still the caller's obligation");
    assert!(server.event_phase(&rx, first));
    server.pump_output(Duration::from_millis(20), &rx);
    assert!(server.panes[&id].live_text().contains("after-handlers"));
}

#[test]
fn a_rejected_preview_wake_runs_the_existing_reload_handler() {
    let (tx, rx) = queue::channel(1);
    let mut server = tests::server(tx);
    let path = std::env::temp_dir().join(format!("kraal-overflow-preview-{}", std::process::id()));
    std::fs::write(&path, "preview-last-change").unwrap();
    let spec = PaneSpec { preview: Some(path.clone()), ..Default::default() };
    let id = server.spawn_pane(None, spec).unwrap();
    server.tx.pane_ready(999);
    assert!(server.tx.wake());
    assert!(server.event_phase(&rx, rx.wait(Duration::ZERO)));
    assert!(server.preview_loads.contains(&id), "Wake started the reload without a tick");
    // Only the requested load remains; it returns through the ordinary payload queue.
    let first = rx.wait(Duration::from_secs(5));
    assert!(matches!(first, Wait::Event(Event::Job(..))));
    assert!(server.event_phase(&rx, first));
    assert!(server.panes[&id].live_text().contains("preview-last-change"));
    server.panes.remove(&id);
    std::fs::remove_file(path).unwrap();
}
