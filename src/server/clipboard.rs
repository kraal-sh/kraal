//! One copy, one destination and one transport. Results belong to the initiating connection.

use std::time::{Duration, Instant};

use super::{ClientId, Server, frame_bytes};
use crate::protocol::{ClipboardOrigin as Origin, ClipboardOutcome as Outcome, ServerMsg};
use crate::theme::NoticeKind;

pub(super) const RESULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct Clipboard {
    pub negotiated: bool,
    next: u64,
    pub pending: Option<(u64, Origin, Instant)>,
    last: Option<Instant>,
    diagnosed: bool,
    refused: bool,
    toast_before: Option<Instant>,
}

impl Clipboard {
    fn prepare(
        &mut self,
        origin: Origin,
        text: &str,
        limit: usize,
        now: Instant,
    ) -> Result<Vec<u8>, (NoticeKind, &'static str)> {
        if text.len() > limit.min(crate::clipboard::MAX_BYTES) {
            return Err((NoticeKind::Error, "copy too large; raise clipboard.max_bytes on the server"));
        }
        if self.pending.is_some() || self.last.is_some_and(|at| now - at < crate::clipboard::INTERVAL) {
            return Err((NoticeKind::Warning, Outcome::Busy.message()));
        }
        self.last = Some(now);
        if self.negotiated {
            self.next = self.next.wrapping_add(1);
            self.pending = Some((self.next, origin, now));
            let mut frame = Vec::new();
            crate::protocol::write_json(
                &mut frame,
                &ServerMsg::Clipboard { id: self.next, origin, text: text.to_string() },
            )
            .expect("bounded clipboard frame");
            Ok(frame)
        } else {
            Ok(frame_bytes(&crate::pane::osc52(text)))
        }
    }
}

impl Server {
    pub(super) fn send_copy(&mut self, client: ClientId, origin: Origin, text: &str) {
        let Some(c) = self.clients.get_mut(&client).filter(|c| c.attached) else { return };
        let state = &mut c.clipboard;
        let frame = match state.prepare(origin, text, self.config.clipboard.max_bytes, Instant::now()) {
            Ok(frame) => frame,
            Err((kind, error)) => {
                let say = origin == Origin::User || !state.refused;
                state.refused = true;
                if say {
                    self.toast(client, kind, error);
                }
                return;
            }
        };
        state.toast_before = c.toast.as_ref().map(|(_, until, _)| *until);
        let sent = c.out.try_send(frame).is_ok();
        if !sent {
            state.pending = None;
            self.toast(client, NoticeKind::Error, "copy not sent: try again");
        } else if !state.negotiated && origin == Origin::User {
            self.toast(client, NoticeKind::Notice, "copy sent to terminal");
        }
    }

    pub(super) fn clipboard_result(&mut self, client: ClientId, id: u64, outcome: Outcome, unavailable: bool) {
        let Some(c) = self.clients.get_mut(&client).filter(|c| c.attached && c.clipboard.negotiated) else { return };
        let state = &mut c.clipboard;
        let Some((pending, origin, _)) = state.pending else { return };
        if pending != id {
            return;
        }
        state.pending = None;
        let unchanged = c.toast.is_none() || c.toast.as_ref().map(|(_, until, _)| *until) == state.toast_before;
        let diagnose = unavailable && !state.diagnosed;
        state.diagnosed |= unavailable;
        if diagnose {
            self.log_message(&format!(
                "client {client}: native clipboard unavailable; using OSC 52. Check local clipboard tools and graphical session access"
            ), NoticeKind::Warning);
        }
        let kind = match outcome {
            Outcome::Native | Outcome::NativeStarted | Outcome::Terminal => NoticeKind::Notice,
            Outcome::Disabled | Outcome::Busy => NoticeKind::Warning,
            Outcome::TooLarge | Outcome::Failed | Outcome::Unknown => NoticeKind::Error,
        };
        if origin == Origin::User || matches!(outcome, Outcome::TooLarge | Outcome::Failed | Outcome::Unknown) {
            if unchanged {
                self.toast(client, kind, outcome.message());
            } else {
                self.log_message(outcome.message(), kind);
            }
        }
    }

    pub(super) fn expire_copies(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .clients
            .values_mut()
            .filter_map(|c| {
                let (_, origin, at) = c.clipboard.pending?;
                if now < at + RESULT_TIMEOUT {
                    return None;
                }
                c.clipboard.pending = None;
                let unchanged =
                    c.toast.is_none() || c.toast.as_ref().map(|(_, until, _)| *until) == c.clipboard.toast_before;
                (origin == Origin::User).then_some((c.id, unchanged))
            })
            .collect();
        for (id, unchanged) in expired {
            let message = "copy result unavailable: try again";
            if unchanged {
                self.toast(id, NoticeKind::Error, message);
            } else {
                self.log_message(message, NoticeKind::Error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{self, ClientMsg};
    use crate::server::{
        queue,
        tests::{client, server},
    };

    #[test]
    fn negotiation_sends_structured_or_legacy_never_both_and_results_are_correlated() {
        for negotiated in [false, true] {
            let (tx, _events) = queue::channel(4096);
            let mut server = server(tx);
            let (id, _peer) = client(&mut server);
            server.clients.get_mut(&id).unwrap().clipboard.negotiated = negotiated;
            server.keep_copy("hello");
            let wire = server
                .clients
                .get_mut(&id)
                .unwrap()
                .clipboard
                .prepare(Origin::User, "hello", 1024, Instant::now())
                .unwrap();
            let mut frames = wire.as_slice();
            let (kind, payload) = protocol::read_frame(&mut frames).unwrap().unwrap();
            if negotiated {
                assert_eq!(kind, protocol::JSON);
                let ServerMsg::Clipboard { id: copy, origin, text } = serde_json::from_slice(&payload).unwrap() else {
                    panic!()
                };
                assert_eq!((origin, text.as_str()), (Origin::User, "hello"));
                assert!(server.clients[&id].toast.is_none(), "no success before the client reports");
                server.clipboard_result(id, copy + 1, Outcome::Native, false);
                assert!(server.clients[&id].toast.is_none());
                server.clipboard_result(id, copy, Outcome::Terminal, true);
                assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, Outcome::Terminal.message());
                assert_eq!(
                    server.messages.iter().filter(|m| m.text.contains("native clipboard unavailable")).count(),
                    1
                );
                server.clipboard_result(id, copy, Outcome::Native, false);
                assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, Outcome::Terminal.message());
            } else {
                assert_eq!((kind, payload), (protocol::BYTES, crate::pane::osc52("hello")));
            }
            assert_eq!(server.buffers.get(None).unwrap().text, "hello");
            assert!(protocol::read_frame(&mut frames).unwrap().is_none(), "a copy must have only one transport");
        }
    }

    #[test]
    fn attach_capability_is_optional_and_only_the_requesting_client_gets_the_copy() {
        let (tx, _events) = queue::channel(4096);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        for features in [vec![], protocol::client_features()] {
            let attach = serde_json::json!({"type":"attach", "version":protocol::PROTOCOL_VERSION,
                "cols":80, "rows":24, "workspace":null, "features":features});
            server.client_frame(id, protocol::JSON, serde_json::to_vec(&attach).unwrap());
            assert_eq!(server.clients[&id].clipboard.negotiated, !features.is_empty());
        }
        let (other, _peer) = client(&mut server);
        server.clients.get_mut(&id).unwrap().clipboard.prepare(Origin::User, "mine", 1024, Instant::now()).unwrap();
        assert!(server.clients[&id].clipboard.pending.is_some());
        assert!(server.clients[&other].clipboard.pending.is_none());
        let wrong = ClientMsg::ClipboardResult { id: 1, outcome: Outcome::Native, native_unavailable: false };
        server.client_frame(other, protocol::JSON, serde_json::to_vec(&wrong).unwrap());
        assert!(server.clients[&id].clipboard.pending.is_some());
    }

    #[test]
    fn a_late_result_keeps_a_newer_notice_and_diagnoses_native_failure_once() {
        let (tx, _events) = queue::channel(4096);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        server.clients.get_mut(&id).unwrap().clipboard.negotiated = true;
        let now = Instant::now();
        for (copy, at) in [(1, now), (2, now + crate::clipboard::INTERVAL)] {
            server.clients.get_mut(&id).unwrap().clipboard.prepare(Origin::User, "text", 1024, at).unwrap();
            server.toast(id, NoticeKind::Error, "the layout could not be saved: Disk full");
            server.clipboard_result(id, copy, Outcome::Terminal, true);
            assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, "the layout could not be saved: Disk full");
        }
        assert_eq!(server.messages.iter().filter(|m| m.text.contains("native clipboard unavailable")).count(), 1);
        assert_eq!(server.messages.iter().filter(|m| m.text == Outcome::Terminal.message()).count(), 2);
        for message in server.messages.iter().filter(|m| m.text == Outcome::Terminal.message()) {
            assert_eq!(message.kind, NoticeKind::Notice);
        }
        server.clients.get_mut(&id).unwrap().mode = crate::server::ui::Mode::Messages { scroll: 0 };
        let screen = server.client_screen(id);
        let mut buf = ratatui::buffer::Buffer::empty(screen);
        server.compose(id, 1_000, &mut buf);
        let mut found = 0;
        for y in 0..screen.height {
            let line: String = (0..screen.width).map(|x| buf[(x, y)].symbol()).collect();
            if let Some(at) = line.find(Outcome::Terminal.message()) {
                let x = line[..at].chars().count() as u16;
                for x in x..x + Outcome::Terminal.message().len() as u16 {
                    assert_eq!(buf[(x, y)].fg, crate::theme::current().text);
                }
                found += 1;
            }
        }
        assert_eq!(found, 2);
        server
            .clients
            .get_mut(&id)
            .unwrap()
            .clipboard
            .prepare(Origin::User, "text", 1024, now + Duration::from_secs(1))
            .unwrap();
        server.expire_copies(now + RESULT_TIMEOUT + Duration::from_secs(1));
        assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, "the layout could not be saved: Disk full");
    }

    #[test]
    fn completed_copies_are_rate_limited_and_unknown_results_never_claim_success() {
        let mut state = Clipboard { negotiated: true, ..Default::default() };
        let now = Instant::now();
        state.prepare(Origin::Program, "first", 1024, now).unwrap();
        state.pending = None;
        assert_eq!(
            state.prepare(Origin::Program, "next", 1024, now).unwrap_err(),
            (NoticeKind::Warning, Outcome::Busy.message())
        );
        assert!(state.prepare(Origin::Program, "next", 1024, now + crate::clipboard::INTERVAL).is_ok());
        assert_eq!(Outcome::Unknown.message(), Outcome::Failed.message());
    }

    #[test]
    fn oversized_busy_and_unacknowledged_copies_preserve_buffers_without_fallback() {
        let (tx, _events) = queue::channel(4096);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        server.clients.get_mut(&id).unwrap().clipboard.negotiated = true;
        server.config.clipboard.max_bytes = 5;
        server.copy_to_client(id, "too big");
        assert!(server.clients[&id].toast.as_ref().unwrap().0.contains("too large"));
        assert_eq!(server.buffers.get(None).unwrap().text, "too big");
        server.clients.get_mut(&id).unwrap().clipboard.prepare(Origin::User, "one", 5, Instant::now()).unwrap();
        server.copy_to_client(id, "two");
        assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, Outcome::Busy.message());
        assert_eq!(server.buffers.get(None).unwrap().text, "two");
        server.clients.get_mut(&id).unwrap().toast = None;
        server.expire_copies(Instant::now() + RESULT_TIMEOUT);
        assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, "copy result unavailable: try again");
        server.clipboard_result(id, 1, Outcome::Native, false);
        assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, "copy result unavailable: try again");
        assert!(server.clients[&id].clipboard.pending.is_none());
    }
}
