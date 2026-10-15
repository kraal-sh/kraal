//! One-use grants and routing for the laptop copy job. No file contents are handled here.

use std::fs::File;
use std::io::Read;
use std::time::Instant;

use crate::files::{FilesAction, Location};
use crate::laptop::copy::{FromClient, Operation, Packet, Spec, TIMEOUT, TOKEN_ENV, ToClient, Window};
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
    prepared: bool,
    removing: bool,
}

impl Copy {
    fn sender(&self) -> Option<ClientId> {
        if self.spec.to_laptop { self.job } else { Some(self.owner) }
    }
    fn receiver(&self) -> Option<ClientId> {
        if self.spec.to_laptop { Some(self.owner) } else { self.job }
    }
}

impl Server {
    pub(super) fn laptop_files_action(&mut self, client: ClientId, action: FilesAction) -> Mode {
        let operation = match &action {
            FilesAction::Move { .. } => Operation::Move,
            FilesAction::Delete(_) => Operation::Delete,
            _ => Operation::Copy,
        };
        if let FilesAction::Delete(targets) = &action {
            let Some(identity) = self.clients.get(&client).and_then(|c| c.laptop.as_ref()) else { return Mode::Normal };
            let owner = identity.id.clone();
            let sources: Option<Vec<_>> = targets
                .iter()
                .map(|s| match s {
                    Location::Laptop { owner_id, path, .. } if owner_id == &owner => Some(path.clone()),
                    _ => None,
                })
                .collect();
            if let Some(sources) = sources.filter(|paths| !paths.is_empty()) {
                self.start_laptop_copy(
                    client,
                    &owner,
                    Spec {
                        operation,
                        to_laptop: false,
                        remote: None,
                        sources,
                        destination: Default::default(),
                        allow: Vec::new(),
                    },
                );
                return Mode::Normal;
            }
        }
        let (sources, dest, replaces) = match action {
            FilesAction::Copy { sources, dest, replaces } | FilesAction::Move { sources, dest, replaces } => {
                (sources, dest, replaces)
            }
            _ => return Mode::Normal,
        };
        let remote = sources.iter().chain([&dest]).find_map(|location| match location {
            Location::Remote(remote) => Some(remote.clone()),
            _ => None,
        });
        if remote.is_some() && operation == Operation::Move {
            self.toast(client, NoticeKind::Error, crate::files::LAPTOP_SERVER_MOVE);
            return Mode::Normal;
        }
        let Some(identity) = self.clients.get(&client).and_then(|c| c.laptop.as_ref()) else { return Mode::Normal };
        let owner = identity.id.clone();
        let to_laptop = matches!(&dest, Location::Laptop { owner_id, .. } if owner_id == &owner);
        let paths: Option<Vec<_>> = sources
            .iter()
            .map(|source| match source {
                Location::Local(path) if to_laptop && remote.is_none() && path.is_absolute() => {
                    path.to_str().map(str::to_owned)
                }
                Location::Remote(r)
                    if to_laptop
                        && remote
                            .as_ref()
                            .is_some_and(|expected| expected.server == r.server && expected.ssh_args == r.ssh_args) =>
                {
                    Some(r.path.clone())
                }
                Location::Laptop { owner_id, path, .. } if !to_laptop && owner_id == &owner => Some(path.clone()),
                _ => None,
            })
            .collect();
        let destination = match &dest {
            Location::Local(path) if path.is_absolute() && path.to_str().is_some() => Some(path.clone()),
            Location::Remote(r) => Some(r.path.clone().into()),
            Location::Laptop { path, .. } if to_laptop => Some(path.clone().into()),
            _ => None,
        };
        let (Some(paths), Some(destination)) = (paths.filter(|paths| !paths.is_empty()), destination) else {
            self.toast(
                client,
                NoticeKind::Error,
                "This laptop operation is unavailable: choose a destination in the other panel and retry",
            );
            return Mode::Normal;
        };
        let production = remote.as_ref().and_then(|r| {
            self.known_servers
                .iter()
                .find(|k| k.server.name == r.server)
                .and_then(|k| self.server_production(&k.server))
                .map(|why| format!("{} is production ({why}).", r.server))
        });
        let mut message = format!(
            "{} {} to {}?",
            if operation == Operation::Move { "Move" } else { "Copy" },
            super::fileman::describe(&sources),
            dest.short_display()
        );
        let spec = Spec {
            operation,
            to_laptop,
            remote,
            sources: paths,
            destination,
            allow: if to_laptop { Vec::new() } else { replaces },
        };
        if !spec.allow.is_empty() {
            message.push_str(&format!("\nIt replaces what is there:\n  {}", spec.allow.join("\n  ")));
        }
        if let Some(why) = &production {
            message.push_str(&format!("\n{why}"));
        }
        if !spec.allow.is_empty() || (to_laptop && operation == Operation::Move) || production.is_some() {
            return Mode::confirm(message, ConfirmAction::LaptopCopy { owner, spec });
        }
        self.start_laptop_copy(client, &owner, spec);
        Mode::Normal
    }

    pub(super) fn start_laptop_copy(&mut self, client: ClientId, owner: &str, spec: Spec) {
        if !self.clients.get(&client).is_some_and(|c| c.attached && c.laptop.as_ref().is_some_and(|l| l.id == owner)) {
            self.toast(client, NoticeKind::Error, "The laptop is not attached: reconnect and retry the operation");
            return;
        }
        if let Some(remote) = &spec.remote {
            if spec.operation != Operation::Copy {
                self.toast(client, NoticeKind::Error, crate::files::LAPTOP_SERVER_MOVE);
                return;
            }
            if self.panel_ssh_args(&remote.server).as_ref() != Some(&remote.ssh_args) {
                self.toast(
                    client,
                    NoticeKind::Warning,
                    format!(
                        "{} is not reached as when its panel was listed: refresh the panel and retry",
                        remote.server
                    ),
                );
                return;
            }
        }
        let mut bytes = [0u8; 32];
        if File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
            self.toast(client, NoticeKind::Error, "Cannot create a laptop copy token: retry the copy");
            return;
        }
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if let Some(pane) = self.start_job_env(
            client,
            match spec.operation {
                Operation::Copy => "copy",
                Operation::Move => "move",
                Operation::Delete => "delete",
            },
            vec!["laptop-cp".into()],
            vec![(TOKEN_ENV.into(), token.clone())],
        ) {
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
                    prepared: false,
                    removing: false,
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
            if copy.spec.to_laptop && copy.prepared || copy.spec.operation != Operation::Copy {
                for pane in self.panes.values_mut() {
                    if let crate::pane::Content::Files(files) = &mut pane.content {
                        if copy.spec.to_laptop {
                            files.refresh_laptop(copy.owner, &copy.spec.destination.to_string_lossy());
                        } else {
                            for path in &copy.spec.sources {
                                if let Some(parent) = std::path::Path::new(path).parent().and_then(|p| p.to_str()) {
                                    files.refresh_laptop(copy.owner, parent);
                                }
                            }
                        }
                    }
                }
                self.mark_dirty();
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
                let mut spec = copy.spec.clone();
                self.copy_send(client, &token, ToClient::Ready(spec.clone()));
                if !spec.to_laptop && spec.remote.is_none() {
                    spec.destination = crate::paths::shorten_home(&spec.destination).into();
                }
                self.copy_send(
                    owner,
                    &token,
                    if spec.operation == Operation::Delete {
                        ToClient::Delete { sources: spec.sources }
                    } else if spec.to_laptop {
                        ToClient::Write(spec)
                    } else if spec.operation == Operation::Move {
                        ToClient::Move(spec)
                    } else {
                        ToClient::Start { sources: spec.sources }
                    },
                );
            }
            FromClient::Data { sequence, packet }
                if Some(client) == copy.sender() && copy.job.is_some() && copy.spec.operation != Operation::Delete =>
            {
                if (copy.spec.to_laptop && !copy.prepared)
                    || copy.done.is_some()
                    || !packet.bounded()
                    || copy.window.sent().ok() != Some(sequence)
                {
                    self.copy_finish(&token, Err("Invalid laptop copy data: retry the copy".into()));
                    return;
                }
                if matches!(packet, Packet::Done) {
                    copy.done = Some(sequence);
                }
                let receiver = copy.receiver().unwrap();
                self.copy_send(receiver, &token, ToClient::Data { sequence, packet });
            }
            FromClient::Ack(sequence)
                if Some(client) == copy.receiver()
                    && copy.job.is_some()
                    && copy.spec.operation != Operation::Delete =>
            {
                if copy.spec.to_laptop && !copy.prepared {
                    if sequence == 0 {
                        copy.prepared = true;
                        copy.progress = Instant::now();
                        let job = copy.job.unwrap();
                        self.copy_send(job, &token, ToClient::Ack(0));
                    }
                    return;
                }
                if copy.window.acknowledge(sequence).is_err() {
                    self.copy_finish(&token, Err("Invalid laptop copy acknowledgement: retry the copy".into()));
                    return;
                }
                copy.progress = Instant::now();
                copy.installing = copy.done == Some(sequence);
                let sender = copy.sender().unwrap();
                self.copy_send(sender, &token, ToClient::Ack(sequence));
            }
            FromClient::Progress
                if (Some(client) == copy.job
                    && copy.spec.to_laptop
                    && copy.spec.remote.is_some()
                    && copy.prepared
                    && copy.done.is_none())
                    || (Some(client) == copy.receiver() && copy.installing)
                    || (Some(client) == copy.sender() && copy.removing)
                    || (copy.spec.operation == Operation::Delete && client == copy.owner && copy.job.is_some()) =>
            {
                copy.progress = Instant::now();
                if copy.spec.to_laptop || copy.spec.operation != Operation::Copy {
                    let peer = if client == copy.owner { copy.job.unwrap() } else { copy.owner };
                    self.copy_send(peer, &token, ToClient::Progress);
                }
            }
            FromClient::Finish(result) if Some(client) == copy.job || client == copy.owner => {
                if result.is_ok() {
                    if copy.spec.operation == Operation::Delete {
                        if client != copy.owner || copy.job.is_none() {
                            return;
                        }
                    } else if copy.spec.operation == Operation::Move && copy.removing {
                        if Some(client) != copy.sender() {
                            return;
                        }
                    } else {
                        if Some(client) != copy.receiver() || !copy.installing {
                            return;
                        }
                        if copy.spec.operation == Operation::Move {
                            copy.removing = true;
                            copy.progress = Instant::now();
                            let sender = copy.sender().unwrap();
                            self.copy_send(sender, &token, ToClient::Installed);
                            return;
                        }
                    }
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
            self.copy_finish(
                &token,
                Err("Laptop operation stalled for 30 seconds: check the affected paths before retrying".into()),
            );
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
            self.copy_finish(
                &token,
                Err("Laptop connection lost: reconnect and check the affected paths before retrying".into()),
            );
        }
    }

    pub(super) fn cancel_laptop_pane(&mut self, pane: PaneId) {
        let tokens: Vec<_> =
            self.laptop_copies.iter().filter(|(_, c)| c.pane == pane).map(|(t, _)| t.clone()).collect();
        for token in tokens {
            self.copy_finish(
                &token,
                Err("Laptop operation cancelled: check the affected paths before retrying".into()),
            );
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
                    operation: crate::laptop::copy::Operation::Copy,
                    to_laptop: false,
                    remote: None,
                    sources: vec!["~/report".into()],
                    destination: "/destination".into(),
                    allow: vec!["report".into()],
                },
                window: Window::new(),
                progress: Instant::now(),
                done: None,
                installing: false,
                prepared: false,
                removing: false,
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
    fn ssh_relay_confirms_production_and_replacements_and_rechecks_identity_after_confirmation() {
        for to_laptop in [false, true] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = server(tx);
            let (owner, _peer) = client(&mut server);
            server.clients.get_mut(&owner).unwrap().laptop =
                Some(crate::laptop::Identity { id: "abc".into(), host: "travel".into(), os: "linux".into() });
            server.known_servers.push(crate::servers::Known {
                server: crate::servers::Server {
                    name: "web-prod".into(),
                    host: "web.lan".into(),
                    ..Default::default()
                },
                source: crate::servers::Source::Inventory,
            });
            let remote = crate::files::Remote {
                server: "web-prod".into(),
                ssh_args: server.panel_ssh_args("web-prod").unwrap(),
                path: "/files".into(),
            };
            let laptop = Location::Laptop { owner_id: "abc".into(), host: "travel".into(), path: "~/Downloads".into() };
            let (source, dest) = if to_laptop {
                (Location::Remote(remote.clone()).join("report"), laptop)
            } else {
                (laptop.join("report"), Location::Remote(remote.clone()))
            };
            let mode = server.laptop_files_action(
                owner,
                FilesAction::Copy {
                    sources: vec![source.clone()],
                    dest: dest.clone(),
                    replaces: vec!["report".into()],
                },
            );
            let Mode::Confirm(question) = mode else { panic!("production must ask before creating a job") };
            assert!(question.message.contains("web-prod is production"));
            assert_eq!(question.message.contains("It replaces"), !to_laptop);
            assert_eq!(question.action.verb(), if to_laptop { "Copy" } else { "Copy and replace" });
            assert!(!question.yes);
            let ConfirmAction::LaptopCopy { owner: identity, spec } = question.action else {
                panic!("laptop confirmation")
            };
            assert_eq!(spec.remote.as_ref().unwrap().ssh_args, remote.ssh_args);
            assert_eq!(spec.to_laptop, to_laptop);
            assert_eq!(spec.allow.is_empty(), to_laptop);
            assert!(server.laptop_copies.is_empty());
            server.known_servers.last_mut().unwrap().server.host = "other.lan".into();
            server.start_laptop_copy(owner, &identity, spec);
            assert!(server.laptop_copies.is_empty(), "changed identity cannot mint a grant");
            assert!(matches!(
                server.laptop_files_action(owner, FilesAction::Move { sources: vec![source], dest, replaces: vec![] }),
                Mode::Normal
            ));
            assert!(server.laptop_copies.is_empty(), "SSH relay never moves sources");
        }
    }

    #[test]
    fn ssh_download_progress_is_routed_only_after_laptop_preparation() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _a) = client(&mut server);
        let (job, _b) = client(&mut server);
        let owner_out = server.clients.get_mut(&owner).unwrap().out.capture();
        let job_out = server.clients.get_mut(&job).unwrap().out.capture();
        grant(&mut server, owner, "abc");
        let spec = &mut server.laptop_copies.get_mut("abc").unwrap().spec;
        spec.to_laptop = true;
        spec.remote =
            Some(crate::files::Remote { server: "web".into(), ssh_args: vec!["web".into()], path: "/files".into() });
        open(&mut server, job, "abc");
        take(&job_out);
        take(&owner_out);
        let old = Instant::now() - TIMEOUT;
        server.laptop_copies.get_mut("abc").unwrap().progress = old;
        server.laptop_copy_message(job, "abc".into(), FromClient::Progress);
        assert_eq!(server.laptop_copies["abc"].progress, old);
        server.laptop_copy_message(owner, "abc".into(), FromClient::Ack(0));
        take(&job_out);
        server.laptop_copies.get_mut("abc").unwrap().progress = old;
        server.laptop_copy_message(owner, "abc".into(), FromClient::Progress);
        assert_eq!(server.laptop_copies["abc"].progress, old);
        server.laptop_copy_message(job, "abc".into(), FromClient::Progress);
        assert!(server.laptop_copies["abc"].progress > old);
        assert!(matches!(take(&owner_out).1, ToClient::Progress));
        server.expire_laptop_copies(old + TIMEOUT);
        assert!(server.laptop_copies.contains_key("abc"));
        server.cancel_laptop_client(owner);
        assert!(matches!(take(&job_out).1, ToClient::Finish(Err(_))));
        assert!(server.laptop_copies.is_empty());
    }

    #[test]
    fn move_requires_installation_then_source_completion_in_both_directions() {
        for to_laptop in [false, true] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = server(tx);
            let (owner, _a) = client(&mut server);
            let (job, _b) = client(&mut server);
            let owner_out = server.clients.get_mut(&owner).unwrap().out.capture();
            let job_out = server.clients.get_mut(&job).unwrap().out.capture();
            grant(&mut server, owner, "abc");
            let copy = server.laptop_copies.get_mut("abc").unwrap();
            copy.spec.operation = Operation::Move;
            copy.spec.to_laptop = to_laptop;
            open(&mut server, job, "abc");
            take(&job_out);
            assert!(matches!(take(&owner_out).1, ToClient::Move(_) | ToClient::Write(_)));
            let (sender, receiver, sender_out, receiver_out) = if to_laptop {
                server.laptop_copy_message(owner, "abc".into(), FromClient::Ack(0));
                take(&job_out);
                (job, owner, &job_out, &owner_out)
            } else {
                (owner, job, &owner_out, &job_out)
            };
            for client in [sender, receiver] {
                server.laptop_copy_message(client, "abc".into(), FromClient::Finish(Ok(())));
                assert!(!server.laptop_copies["abc"].removing);
            }
            server.laptop_copy_message(sender, "abc".into(), FromClient::Data { sequence: 0, packet: Packet::Done });
            take(receiver_out);
            server.laptop_copy_message(receiver, "abc".into(), FromClient::Ack(0));
            take(sender_out);
            server.laptop_copy_message(sender, "abc".into(), FromClient::Finish(Ok(())));
            assert!(!server.laptop_copies["abc"].removing);
            server.laptop_copy_message(receiver, "abc".into(), FromClient::Progress);
            assert!(matches!(take(sender_out).1, ToClient::Progress));
            server.laptop_copy_message(receiver, "abc".into(), FromClient::Finish(Ok(())));
            assert!(matches!(take(sender_out).1, ToClient::Installed));
            assert!(receiver_out.try_recv().is_err());
            server.laptop_copy_message(receiver, "abc".into(), FromClient::Finish(Ok(())));
            assert!(server.laptop_copies.contains_key("abc"));
            server.laptop_copy_message(sender, "abc".into(), FromClient::Finish(Ok(())));
            assert!(matches!(take(sender_out).1, ToClient::Finish(Ok(()))));
            assert!(matches!(take(receiver_out).1, ToClient::Finish(Ok(()))));
            assert!(server.laptop_copies.is_empty());
        }
    }

    #[test]
    fn delete_is_sent_only_to_its_owner_and_only_that_owner_can_complete_it() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _a) = client(&mut server);
        let (job, _b) = client(&mut server);
        let (other, _c) = client(&mut server);
        let owner_out = server.clients.get_mut(&owner).unwrap().out.capture();
        let job_out = server.clients.get_mut(&job).unwrap().out.capture();
        let other_out = server.clients.get_mut(&other).unwrap().out.capture();
        grant(&mut server, owner, "abc");
        server.laptop_copies.get_mut("abc").unwrap().spec.operation = Operation::Delete;
        open(&mut server, job, "abc");
        assert!(matches!(take(&job_out).1, ToClient::Ready(spec) if spec.operation == Operation::Delete));
        assert!(matches!(take(&owner_out).1, ToClient::Delete { sources } if sources == ["~/report"]));
        for client in [job, other] {
            server.laptop_copy_message(client, "abc".into(), FromClient::Finish(Ok(())));
            assert!(server.laptop_copies.contains_key("abc"));
        }
        server.laptop_copy_message(owner, "abc".into(), FromClient::Finish(Ok(())));
        assert!(matches!(take(&job_out).1, ToClient::Finish(Ok(()))));
        assert!(matches!(take(&owner_out).1, ToClient::Finish(Ok(()))));
        assert!(other_out.try_recv().is_err());
    }

    #[test]
    fn move_destination_is_shortened_only_for_the_laptop_owner() {
        let home = dirs::home_dir().unwrap();
        for (destination, shown) in
            [(home.clone(), "~"), (home.join("work"), "~/work"), ("/outside/work".into(), "/outside/work")]
        {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = server(tx);
            let (owner, _a) = client(&mut server);
            let (job, _b) = client(&mut server);
            let owner_out = server.clients.get_mut(&owner).unwrap().out.capture();
            let job_out = server.clients.get_mut(&job).unwrap().out.capture();
            grant(&mut server, owner, "abc");
            let spec = &mut server.laptop_copies.get_mut("abc").unwrap().spec;
            spec.operation = Operation::Move;
            spec.destination = destination.clone();
            open(&mut server, job, "abc");
            assert!(matches!(take(&job_out).1, ToClient::Ready(spec) if spec.destination == destination));
            assert!(
                matches!(take(&owner_out).1, ToClient::Move(spec) if spec.destination == std::path::Path::new(shown))
            );
            assert_eq!(server.laptop_copies["abc"].spec.destination, destination);
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
    #[test]
    fn moving_to_laptop_confirms_the_session_sources_and_labels_the_action_move() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _a) = client(&mut server);
        server.clients.get_mut(&owner).unwrap().laptop =
            Some(crate::laptop::Identity { id: "abc".into(), host: "travel".into(), os: "linux".into() });
        let mode = server.laptop_files_action(
            owner,
            FilesAction::Move {
                sources: vec![Location::Local("/server/report".into())],
                dest: Location::Laptop { owner_id: "abc".into(), host: "travel".into(), path: "~/Downloads".into() },
                replaces: vec![],
            },
        );
        let Mode::Confirm(question) = mode else { panic!("move must ask first") };
        assert_eq!(question.message, "Move /server/report to travel:~/Downloads?");
        assert_eq!(question.action.verb(), "Move");
        assert!(!question.yes);
        assert!(server.laptop_copies.is_empty());
        assert!(
            matches!(question.action, ConfirmAction::LaptopCopy { spec, .. } if spec.operation == Operation::Move && spec.to_laptop)
        );
    }

    #[test]
    fn reverse_copy_routes_only_job_data_and_client_acknowledgements_and_completion() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _a) = client(&mut server);
        let (job, _b) = client(&mut server);
        let owner_out = server.clients.get_mut(&owner).unwrap().out.capture();
        let job_out = server.clients.get_mut(&job).unwrap().out.capture();
        grant(&mut server, owner, "abc");
        server.laptop_copies.get_mut("abc").unwrap().spec.to_laptop = true;
        server.laptop_copy_message(owner, "abc".into(), FromClient::Ack(0));
        assert!(!server.laptop_copies["abc"].prepared);
        open(&mut server, job, "abc");
        assert!(matches!(
            take(&job_out).1,
            ToClient::Ready(Spec { operation: crate::laptop::copy::Operation::Copy, to_laptop: true, .. })
        ));
        assert!(matches!(take(&owner_out).1, ToClient::Write(_)));
        server.laptop_copy_message(job, "abc".into(), FromClient::Ack(0));
        assert!(job_out.try_recv().is_err());
        server.laptop_copy_message(owner, "abc".into(), FromClient::Ack(0));
        assert!(matches!(take(&job_out).1, ToClient::Ack(0)));
        server.laptop_copy_message(owner, "abc".into(), FromClient::Data { sequence: 0, packet: Packet::Done });
        assert!(job_out.try_recv().is_err() && owner_out.try_recv().is_err());
        server.laptop_copy_message(job, "abc".into(), FromClient::Data { sequence: 0, packet: Packet::Done });
        assert!(matches!(take(&owner_out).1, ToClient::Data { sequence: 0, packet: Packet::Done }));
        server.laptop_copy_message(owner, "abc".into(), FromClient::Ack(0));
        assert!(matches!(take(&job_out).1, ToClient::Ack(0)));
        server.laptop_copy_message(owner, "abc".into(), FromClient::Progress);
        assert!(matches!(take(&job_out).1, ToClient::Progress));
        server.laptop_copy_message(job, "abc".into(), FromClient::Finish(Ok(())));
        assert!(server.laptop_copies.contains_key("abc"));
        server.laptop_copy_message(owner, "abc".into(), FromClient::Finish(Ok(())));
        assert!(matches!(take(&job_out).1, ToClient::Finish(Ok(()))));
        assert!(matches!(take(&owner_out).1, ToClient::Finish(Ok(()))));
        assert!(server.laptop_copies.is_empty());
    }
}
