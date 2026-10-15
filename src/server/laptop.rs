//! One-use grants and routing for the laptop copy job. No file contents are handled here.

use std::time::Instant;

use crate::files::{FilesAction, Location};
use crate::laptop::copy::{FromClient, Packet, Spec, TIMEOUT, TOKEN_ENV, ToClient, Window};
use crate::layout::PaneId;
use crate::protocol::ServerMsg;
use crate::theme::NoticeKind;

use super::ui::{ConfirmAction, Mode};
use super::{ClientId, Server};

pub(super) struct Copy {
    owner: ClientId,
    pane: PaneId,
    job: Option<ClientId>,
    spec: Spec,
    window: Window,
    progress: Instant,
    done: Option<u64>,
    installing: bool,
}

impl Server {
    pub(super) fn laptop_files_action(&mut self, client: ClientId, action: FilesAction) -> Mode {
        if let FilesAction::Copy { sources, dest: Location::Local(destination), replaces } = action {
            if destination.to_str().is_none() || !destination.is_absolute() {
                self.toast(
                    client,
                    NoticeKind::Error,
                    "Choose an absolute destination with a valid UTF-8 path and retry the copy",
                );
                return Mode::Normal;
            }
            let Some(identity) = self.clients.get(&client).and_then(|c| c.laptop.as_ref()) else { return Mode::Normal };
            let owner = identity.id.clone();
            let paths: Option<Vec<_>> = sources
                .iter()
                .map(|s| match s {
                    Location::Laptop { owner_id, path, .. } if owner_id == &owner => Some(path.clone()),
                    _ => None,
                })
                .collect();
            if let Some(sources) = paths.filter(|p| !p.is_empty()) {
                let spec = Spec { sources, destination, allow: replaces };
                if spec.allow.is_empty() {
                    self.start_laptop_copy(client, &owner, spec);
                } else {
                    let message = format!(
                        "Copy {} items to {}?\nIt replaces what is there:\n  {}",
                        spec.sources.len(),
                        spec.destination.display(),
                        spec.allow.join("\n  ")
                    );
                    return Mode::confirm(message, ConfirmAction::LaptopCopy { owner, spec });
                }
                return Mode::Normal;
            }
        }
        self.toast(client, NoticeKind::Error, "This laptop operation is unavailable: copy from your laptop to the session's machine, or run kraal cp on the laptop");
        Mode::Normal
    }

    pub(super) fn start_laptop_copy(&mut self, client: ClientId, owner: &str, spec: Spec) {
        if !self.clients.get(&client).is_some_and(|c| c.attached && c.laptop.as_ref().is_some_and(|l| l.id == owner)) {
            self.toast(client, NoticeKind::Error, "The laptop is not attached: reconnect and retry the copy");
            return;
        }
        let mut bytes = [0u8; 32];
        // getentropy is available on Linux and macOS and does not open a file in the loop.
        if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
            self.toast(client, NoticeKind::Error, "Cannot create a laptop copy token: retry the copy");
            return;
        }
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if let Some(pane) =
            self.start_job_env(client, "copy", vec!["laptop-cp".into()], vec![(TOKEN_ENV.into(), token.clone())])
        {
            // A saved or respawned job must never carry a live grant.
            self.panes.get_mut(&pane).unwrap().spec.env.retain(|(key, _)| key != TOKEN_ENV);
            self.laptop_copies.insert(
                token,
                Copy {
                    owner: client,
                    pane,
                    job: None,
                    spec,
                    window: Window::new(),
                    progress: Instant::now(),
                    done: None,
                    installing: false,
                },
            );
        }
    }

    fn copy_send(&self, client: ClientId, token: &str, message: ToClient) {
        self.send_msg(client, &ServerMsg::LaptopCopy { token: token.into(), message });
    }

    fn copy_finish(&mut self, token: &str, result: Result<(), String>) {
        if let Some(copy) = self.laptop_copies.remove(token) {
            self.copy_send(copy.owner, token, ToClient::Finish(result.clone()));
            if let Some(job) = copy.job {
                self.copy_send(job, token, ToClient::Finish(result));
            }
        }
    }

    pub(super) fn laptop_copy_message(&mut self, client: ClientId, token: String, message: FromClient) {
        let Some(copy) = self.laptop_copies.get_mut(&token) else {
            if matches!(message, FromClient::Open { .. }) {
                self.copy_send(
                    client,
                    &token,
                    ToClient::Finish(Err("This copy token is no longer valid: retry from the file manager".into())),
                );
            }
            return;
        };
        match message {
            FromClient::Open { version } => {
                if version != crate::protocol::PROTOCOL_VERSION {
                    self.copy_send(client, &token, ToClient::Finish(Err(self.version_mismatch(version))));
                    return;
                }
                if copy.job.is_some() || copy.owner == client || self.clients.get(&client).is_none_or(|c| c.attached) {
                    self.copy_send(
                        client,
                        &token,
                        ToClient::Finish(Err(
                            "This copy token has already been used: retry from the file manager".into()
                        )),
                    );
                    return;
                }
                copy.job = Some(client);
                copy.progress = Instant::now();
                let owner = copy.owner;
                let spec = copy.spec.clone();
                self.copy_send(client, &token, ToClient::Ready(spec.clone()));
                self.copy_send(owner, &token, ToClient::Start { sources: spec.sources });
            }
            FromClient::Data { sequence, packet } if client == copy.owner && copy.job.is_some() => {
                if copy.done.is_some() || !packet.bounded() || copy.window.sent().ok() != Some(sequence) {
                    self.copy_finish(&token, Err("Invalid laptop copy data: retry the copy".into()));
                    return;
                }
                if matches!(packet, Packet::Done) {
                    copy.done = Some(sequence);
                }
                let job = copy.job.unwrap();
                self.copy_send(job, &token, ToClient::Data { sequence, packet });
            }
            FromClient::Ack(sequence) if Some(client) == copy.job => {
                if copy.window.acknowledge(sequence).is_err() {
                    self.copy_finish(&token, Err("Invalid laptop copy acknowledgement: retry the copy".into()));
                    return;
                }
                copy.progress = Instant::now();
                copy.installing = copy.done == Some(sequence);
                let owner = copy.owner;
                self.copy_send(owner, &token, ToClient::Ack(sequence));
            }
            FromClient::Progress if Some(client) == copy.job && copy.installing => {
                copy.progress = Instant::now();
            }
            FromClient::Finish(result) if Some(client) == copy.job || client == copy.owner => {
                if result.is_ok() && (Some(client) != copy.job || !copy.installing) {
                    return;
                }
                self.copy_finish(&token, result);
            }
            _ => {}
        }
    }

    pub(super) fn expire_laptop_copies(&mut self, now: Instant) {
        let tokens: Vec<_> = self
            .laptop_copies
            .iter()
            .filter(|(_, c)| now.saturating_duration_since(c.progress) >= TIMEOUT)
            .map(|(t, _)| t.clone())
            .collect();
        for token in tokens {
            self.copy_finish(&token, Err("Laptop copy stalled for 30 seconds: reconnect and retry the copy".into()));
        }
    }

    pub(super) fn cancel_laptop_client(&mut self, client: ClientId) {
        let tokens: Vec<_> = self
            .laptop_copies
            .iter()
            .filter(|(_, c)| c.owner == client || c.job == Some(client))
            .map(|(t, _)| t.clone())
            .collect();
        for token in tokens {
            self.copy_finish(&token, Err("Laptop connection lost: reconnect and retry the copy".into()));
        }
    }

    pub(super) fn cancel_laptop_pane(&mut self, pane: PaneId) {
        let tokens: Vec<_> =
            self.laptop_copies.iter().filter(|(_, c)| c.pane == pane).map(|(t, _)| t.clone()).collect();
        for token in tokens {
            self.copy_finish(&token, Err("Laptop copy cancelled: select the files and press c to retry".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{self, ClientMsg};
    use crate::server::tests::{client, server};

    fn grant(server: &mut Server, owner: ClientId, token: &str) {
        server.laptop_copies.insert(
            token.into(),
            Copy {
                owner,
                pane: 99,
                job: None,
                spec: Spec {
                    sources: vec!["~/report".into()],
                    destination: "/destination".into(),
                    allow: vec!["report".into()],
                },
                window: Window::new(),
                progress: Instant::now(),
                done: None,
                installing: false,
            },
        );
    }

    fn open(server: &mut Server, job: ClientId, token: &str) {
        server.clients.get_mut(&job).unwrap().attached = false;
        server.laptop_copy_message(job, token.into(), FromClient::Open { version: protocol::PROTOCOL_VERSION });
    }

    fn take(rx: &crossbeam_channel::Receiver<Vec<u8>>) -> (String, ToClient) {
        let frame = rx.recv_timeout(crate::testing::PATIENCE).unwrap();
        let (_, bytes) = protocol::read_frame(&mut frame.as_slice()).unwrap().unwrap();
        match serde_json::from_slice(&bytes).unwrap() {
            ServerMsg::LaptopCopy { token, message } => (token, message),
            other => panic!("unexpected message {other:?}"),
        }
    }

    #[test]
    fn token_is_single_use_and_routes_only_bound_paths_owner_and_job() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _owner_peer) = client(&mut server);
        let (job, _job_peer) = client(&mut server);
        let (other, _other_peer) = client(&mut server);
        let owner_out = server.clients.get_mut(&owner).unwrap().out.capture();
        let job_out = server.clients.get_mut(&job).unwrap().out.capture();
        let other_out = server.clients.get_mut(&other).unwrap().out.capture();
        grant(&mut server, owner, "abc");
        open(&mut server, job, "abc");
        assert!(
            matches!(take(&job_out), (token, ToClient::Ready(spec)) if token == "abc" && spec.sources == ["~/report"] && spec.destination == std::path::Path::new("/destination") && spec.allow == ["report"])
        );
        assert!(matches!(take(&owner_out).1, ToClient::Start { sources } if sources == ["~/report"]));
        open(&mut server, other, "abc");
        assert!(matches!(take(&other_out).1, ToClient::Finish(Err(_))));
        assert_eq!(server.laptop_copies["abc"].job, Some(job));
        let packet = Packet::Begin { bytes: 123, entries: 1 };
        for wrong in [other, job] {
            server.laptop_copy_message(wrong, "abc".into(), FromClient::Data { sequence: 0, packet: packet.clone() });
        }
        assert!(job_out.try_recv().is_err() && owner_out.try_recv().is_err());
        let frame = ClientMsg::LaptopCopy { token: "abc".into(), message: FromClient::Data { sequence: 0, packet } };
        server.client_frame(owner, protocol::JSON, serde_json::to_vec(&frame).unwrap());
        assert!(matches!(take(&job_out).1, ToClient::Data { sequence: 0, packet: Packet::Begin { bytes: 123, .. } }));
        for wrong in [other, owner] {
            server.laptop_copy_message(wrong, "abc".into(), FromClient::Ack(0));
        }
        assert!(owner_out.try_recv().is_err());
        server.laptop_copy_message(job, "abc".into(), FromClient::Ack(0));
        assert!(matches!(take(&owner_out).1, ToClient::Ack(0)));
        server.laptop_copy_message(other, "abc".into(), FromClient::Finish(Err("forged".into())));
        assert!(server.laptop_copies.contains_key("abc"));
        server.laptop_copy_message(owner, "abc".into(), FromClient::Finish(Ok(())));
        assert!(server.laptop_copies.contains_key("abc"));
        server.laptop_copy_message(owner, "abc".into(), FromClient::Data { sequence: 1, packet: Packet::Done });
        take(&job_out);
        server.laptop_copy_message(job, "abc".into(), FromClient::Ack(1));
        take(&owner_out);
        server.laptop_copy_message(job, "abc".into(), FromClient::Finish(Ok(())));
        assert!(matches!(take(&owner_out).1, ToClient::Finish(Ok(()))));
        assert!(matches!(take(&job_out).1, ToClient::Finish(Ok(()))));
        assert!(server.laptop_copies.is_empty());
        open(&mut server, other, "abc");
        assert!(matches!(take(&other_out).1, ToClient::Finish(Err(_))));
    }

    #[test]
    fn relay_rejects_wrong_protocol_out_of_order_data_and_excess_window() {
        for invalid in [0, 1, 2] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = server(tx);
            let (owner, _a) = client(&mut server);
            let (job, _b) = client(&mut server);
            let out = server.clients.get_mut(&job).unwrap().out.capture();
            grant(&mut server, owner, "abc");
            server.clients.get_mut(&job).unwrap().attached = false;
            server.laptop_copy_message(job, "abc".into(), FromClient::Open { version: protocol::PROTOCOL_VERSION - 1 });
            assert!(matches!(take(&out).1, ToClient::Finish(Err(error)) if error.contains("protocol")));
            assert!(server.laptop_copies["abc"].job.is_none());
            open(&mut server, job, "abc");
            take(&out);
            match invalid {
                0 => server.laptop_copy_message(
                    owner,
                    "abc".into(),
                    FromClient::Data { sequence: 1, packet: Packet::End },
                ),
                1 => server.laptop_copy_message(
                    owner,
                    "abc".into(),
                    FromClient::Data { sequence: 0, packet: Packet::Chunk("a".repeat(100_000)) },
                ),
                _ => {
                    for sequence in 0..5 {
                        server.laptop_copy_message(
                            owner,
                            "abc".into(),
                            FromClient::Data { sequence, packet: Packet::End },
                        );
                    }
                }
            }
            assert!(server.laptop_copies.is_empty());
        }
    }

    #[test]
    fn only_receiver_progress_extends_deadline_and_disconnect_or_close_cancels_both_ends() {
        for stop in [0, 1, 2, 3] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = server(tx);
            let (owner, _a) = client(&mut server);
            let (job, _b) = client(&mut server);
            let owner_out = server.clients.get_mut(&owner).unwrap().out.capture();
            let job_out = server.clients.get_mut(&job).unwrap().out.capture();
            grant(&mut server, owner, "abc");
            open(&mut server, job, "abc");
            take(&owner_out);
            take(&job_out);
            let progress = server.laptop_copies["abc"].progress;
            server.laptop_copy_message(owner, "abc".into(), FromClient::Data { sequence: 0, packet: Packet::End });
            take(&job_out);
            assert_eq!(server.laptop_copies["abc"].progress, progress);
            server.expire_laptop_copies(progress + TIMEOUT - std::time::Duration::from_nanos(1));
            assert!(server.laptop_copies.contains_key("abc"));
            match stop {
                0 => server.expire_laptop_copies(progress + TIMEOUT),
                1 => server.drop_client(owner),
                2 => server.drop_client(job),
                _ => server.close_pane(99),
            }
            assert!(server.laptop_copies.is_empty());
            assert!(matches!(take(&owner_out).1, ToClient::Finish(Err(_))));
            assert!(matches!(take(&job_out).1, ToClient::Finish(Err(_))));
        }
    }

    #[test]
    fn a_valid_acknowledgement_renews_the_progress_deadline() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _a) = client(&mut server);
        let (job, _b) = client(&mut server);
        grant(&mut server, owner, "abc");
        open(&mut server, job, "abc");
        let old = Instant::now() - TIMEOUT;
        server.laptop_copies.get_mut("abc").unwrap().progress = old;
        server.laptop_copy_message(owner, "abc".into(), FromClient::Data { sequence: 0, packet: Packet::End });
        server.laptop_copy_message(job, "abc".into(), FromClient::Ack(0));
        server.expire_laptop_copies(old + TIMEOUT);
        assert!(server.laptop_copies.contains_key("abc"));
        let renewed = server.laptop_copies["abc"].progress;
        server.expire_laptop_copies(renewed + TIMEOUT);
        assert!(server.laptop_copies.is_empty());
    }

    #[test]
    fn installation_progress_is_bound_to_the_job_and_stalls_still_expire() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _a) = client(&mut server);
        let (job, _b) = client(&mut server);
        let owner_out = server.clients.get_mut(&owner).unwrap().out.capture();
        let job_out = server.clients.get_mut(&job).unwrap().out.capture();
        grant(&mut server, owner, "abc");
        open(&mut server, job, "abc");
        take(&owner_out);
        take(&job_out);
        let before = server.laptop_copies["abc"].progress;
        server.laptop_copy_message(job, "abc".into(), FromClient::Progress);
        assert_eq!(server.laptop_copies["abc"].progress, before);
        server.laptop_copy_message(owner, "abc".into(), FromClient::Data { sequence: 0, packet: Packet::Done });
        take(&job_out);
        server.laptop_copy_message(job, "abc".into(), FromClient::Ack(0));
        take(&owner_out);
        assert!(server.laptop_copies["abc"].installing);
        let old = Instant::now() - TIMEOUT;
        server.laptop_copies.get_mut("abc").unwrap().progress = old;
        server.laptop_copy_message(owner, "abc".into(), FromClient::Progress);
        assert_eq!(server.laptop_copies["abc"].progress, old);
        server.laptop_copy_message(job, "abc".into(), FromClient::Progress);
        server.expire_laptop_copies(old + TIMEOUT);
        assert!(server.laptop_copies.contains_key("abc"));
        let renewed = server.laptop_copies["abc"].progress;
        server.expire_laptop_copies(renewed + TIMEOUT);
        assert!(server.laptop_copies.is_empty());
        assert!(matches!(take(&owner_out).1, ToClient::Finish(Err(error)) if error.contains("30 seconds")));
        assert!(matches!(take(&job_out).1, ToClient::Finish(Err(error)) if error.contains("30 seconds")));
    }

    #[test]
    fn laptop_replacement_uses_existing_confirmation_and_foreign_owners_cannot_copy() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _a) = client(&mut server);
        server.clients.get_mut(&owner).unwrap().laptop =
            Some(crate::laptop::Identity { id: "abc".into(), host: "travel".into(), os: "linux".into() });
        let action = |id: &str| FilesAction::Copy {
            sources: vec![Location::Laptop { owner_id: id.into(), host: "travel".into(), path: "~/report".into() }],
            dest: Location::Local("/destination".into()),
            replaces: vec!["report".into()],
        };
        assert!(matches!(server.laptop_files_action(owner, action("other")), Mode::Normal));
        assert!(server.laptop_copies.is_empty());
        let mode = server.laptop_files_action(owner, action("abc"));
        assert!(matches!(mode, Mode::Confirm(_)));
        assert!(server.laptop_copies.is_empty(), "a replacement must be confirmed before minting a token");
    }
}
