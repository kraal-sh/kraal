//! `kraal config reload`: read config.toml again while the server runs.

use super::config_scan::Inputs;
use super::requests::{ReplyHandle, RequestOutcome};
use super::{Server, events, frame_bytes};
use crate::accounts::Accounts;
use crate::config::Config;
use crate::keymap::Keymap;
use crate::protocol::Response;
use crate::servers::Inventory;

struct Inventories {
    accounts: anyhow::Result<Accounts>,
    servers: anyhow::Result<Inventory>,
    hosts: Vec<String>,
    hosts_failed: bool,
}

impl Inventories {
    fn read(ssh_hosts: bool, inputs: &mut Inputs) -> Self {
        let (hosts, hosts_failed) = if ssh_hosts {
            super::ssh::ssh_config_hosts_observed(&mut |path, directory| {
                if directory {
                    inputs.directory(path);
                } else {
                    inputs.file(path);
                }
            })
        } else {
            (Vec::new(), false)
        };
        Self { accounts: Accounts::load(), servers: Inventory::load(), hosts, hosts_failed }
    }

    fn failed(&self) -> bool {
        use super::config_scan::retryable;
        self.accounts.as_ref().is_err_and(retryable) || self.servers.as_ref().is_err_and(retryable) || self.hosts_failed
    }
}

impl Server {
    pub(super) fn check_definitions(&mut self) {
        if self.config_scan.upgrade().is_none()
            && (self.config_scan_again
                || self.config_scan_failed
                || self
                    .config_inputs
                    .as_ref()
                    .is_none_or(|inputs| inputs.scope_changed(self) || inputs.changed().unwrap_or(true)))
        {
            self.scan_definitions_async();
        }
    }

    pub(super) fn scan_definitions(&mut self) {
        self.config_generation += 1;
        let mut inputs = Inputs::configuration();
        let definitions = self.prepare_definition_scan().read(&mut inputs);
        let read = Inventories::read(self.config.ssh_config_hosts, &mut inputs);
        self.config_scan_failed = definitions.failed() || read.failed();
        let defs = self.apply_definition_scan(definitions, true);
        self.apply_known(&defs, read.servers, &read.hosts);
        self.apply_accounts(read.accounts);
        inputs.follow_sources(self);
        self.config_inputs = Some(inputs);
    }

    pub(super) fn scan_definitions_async(&mut self) {
        let ssh_hosts = self.config.ssh_config_hosts;
        self.start_inventory_scan(move |inputs| Inventories::read(ssh_hosts, inputs));
    }

    fn start_inventory_scan(&mut self, read: impl FnOnce(&mut Inputs) -> Inventories + Send + 'static) {
        if self.config_scan.upgrade().is_some() {
            self.config_scan_again = true;
            return;
        }
        self.config_scan_again = false;
        self.config_scan_failed = true;
        let definitions = self.prepare_definition_scan();
        let pending = std::rc::Rc::new(());
        self.config_scan = std::rc::Rc::downgrade(&pending);
        let generation = self.config_generation;
        self.background(
            move || {
                let mut inputs = Inputs::configuration();
                let definitions = definitions.read(&mut inputs);
                let read = read(&mut inputs);
                (definitions, read, inputs)
            },
            move |server, (definitions, read, mut inputs)| {
                drop(pending);
                // Only applied edits invalidate reads. Requests for another scan must not
                // starve completed reads while usage notifications keep arriving.
                let stale = generation != server.config_generation;
                let changed = inputs.scope_changed(server) || inputs.changed().unwrap_or(false);
                if !stale {
                    server.config_scan_failed = definitions.failed() || read.failed();
                    let (known, accounts, error) =
                        (server.known_servers.clone(), server.accounts.clone(), server.accounts_error.clone());
                    let defs = server.apply_definition_scan(definitions, !changed);
                    server.apply_known(&defs, read.servers, &read.hosts);
                    server.apply_accounts(read.accounts);
                    inputs.follow_sources(server);
                    server.config_inputs = Some(inputs);
                    // Definitions and their sources mark their own changes.
                    if known != server.known_servers || accounts != server.accounts || error != server.accounts_error {
                        server.mark_dirty();
                    }
                }
                if std::mem::take(&mut server.config_scan_again) || stale || changed {
                    server.scan_definitions_async();
                }
            },
        );
    }

    pub(super) fn reload_config(&mut self, reply: ReplyHandle) -> RequestOutcome {
        self.start_config_reload(reply, Config::load_with_warnings)
    }

    fn start_config_reload(
        &mut self,
        reply: ReplyHandle,
        read: impl FnOnce() -> anyhow::Result<(Config, Vec<String>)> + Send + 'static,
    ) -> RequestOutcome {
        self.reload_generation += 1;
        let generation = self.reload_generation;
        match self.background_owned(reply, read, move |server, reply, result| {
            let result = if generation != server.reload_generation {
                Err("a newer config reload was requested; run config reload again".into())
            } else {
                result
                    .map_err(|e| format!("config: {e:#}"))
                    .map(|(config, warnings)| Response::Text(server.apply_config(config, warnings).join("\n")))
            };
            reply.complete(server, result);
        }) {
            Ok(()) => RequestOutcome::Deferred,
            Err(reply) => {
                RequestOutcome::Immediate { reply, result: Err("cannot start config reload; try again".into()) }
            }
        }
    }

    /// Apply the configuration: keys, theme, sidebar, bottom bar (when its
    /// setting changed: a toggle for the session holds until then), mouse, pane frames,
    /// notifications, tools, hints, hooks, production words, contexts, saving scrollback and the
    /// paste buffers' limits and file. Panes
    /// already open keep their shell and scrollback length; new panes take the new ones. When the
    /// file cannot be read, nothing changes. Returns the warnings.
    fn apply_config(&mut self, config: Config, mut warnings: Vec<String>) -> Vec<String> {
        self.config_generation += 1;
        let (mut keymap, w) = Keymap::build(&config.keys);
        warnings.extend(w);
        warnings.extend(keymap.bind_tools(&config.tools));
        warnings.extend(crate::tools::check(&config.tools));
        let (hint_patterns, w) = crate::hints::Custom::build(&config.hints, &config.tools);
        warnings.extend(w);
        let (hook_list, w) = events::read_hooks(&config.hooks);
        warnings.extend(w);
        let (theme, w) = crate::theme::Theme::from_config(&config.theme);
        warnings.extend(w);
        crate::theme::set(theme);
        self.keymap = keymap;
        self.hint_patterns = hint_patterns;
        // Hooks that are running keep their supervision; the new list applies to the next events.
        self.hooks.list = hook_list;
        self.env.shell = config.shell();
        self.env.scrollback = config.scrollback;
        // Attached terminals report the mouse, or stop, as the new setting says.
        if config.mouse != self.config.mouse {
            let modes = if config.mouse { crate::client::MOUSE_ON } else { crate::client::MOUSE_OFF };
            for c in self.clients.values().filter(|c| c.attached) {
                let _ = c.out.try_send(frame_bytes(modes.as_bytes()));
            }
        }
        if config.bar.keys != self.config.bar.keys {
            self.bottom_bar = config.bar.keys != crate::config::BarKeys::Off;
        }
        let persisted = self.config.buffers.persist;
        self.config = config;
        self.reconfigure_buffers(persisted);
        // Workspace folders, the sidebar's width, the bottom bar and pane frames may have changed.
        self.scan_definitions_async();
        self.relayout();
        self.mark_dirty();
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Request;
    use crate::server::requests::tests::{finish_job, response};
    use crate::server::tests::{client, server};
    use crate::testing::PATIENCE;

    fn until(server: &mut Server, rx: &crate::server::queue::EventReceiver, done: impl Fn(&Server) -> bool) {
        let deadline = std::time::Instant::now() + PATIENCE;
        while !done(server) {
            let remaining = deadline.checked_duration_since(std::time::Instant::now()).expect("jobs did not finish");
            server.handle(rx.recv_timeout(remaining).expect("jobs did not finish"));
        }
    }

    fn one_scan(server: &mut Server, rx: &crate::server::queue::EventReceiver) {
        let before = server.next_id;
        server.check_definitions();
        assert!(server.config_scan.upgrade().is_some());
        until(server, rx, |s| s.config_scan.upgrade().is_none());
        assert_eq!(server.next_id, before + 1, "one scan per change");
        for _ in 0..3 {
            server.check_definitions();
        }
        assert_eq!(server.next_id, before + 1, "unchanged inputs must not start a job");
    }

    #[test]
    fn every_scanned_file_triggers_one_scan_per_change() {
        if !crate::testing::in_own_process("server::reload::tests::every_scanned_file_triggers_one_scan_per_change") {
            return;
        }
        let home = crate::paths::home_dir().unwrap();
        let extra = home.join("extra");
        let external = home.join("external/source.kdl");
        let ssh = home.join(".ssh");
        for dir in
            [crate::paths::workspaces_dir(), extra.clone(), external.parent().unwrap().into(), ssh.join("config.d")]
        {
            std::fs::create_dir_all(dir).unwrap();
        }
        let paths = [
            (crate::paths::config_path(), "scrollback = 100\n", false),
            (Accounts::path(), "[limits]\nthreshold = 80\n", false),
            (Inventory::path(), "[[server]]\nname = 'box'\nhost = 'box.test'\n", false),
            (crate::paths::workspaces_dir().join("one.kdl"), "layout { pane; }\n", false),
            (extra.join("two.kdl"), "layout { pane; }\n", false),
            (external, "layout { pane; }\n", true),
            (ssh.join("config"), "Host box\n", false),
            (ssh.join("included"), "Host included\n", false),
            (ssh.join("nested"), "Host nested\n", false),
            (ssh.join("config.d/one"), "Host globbed\n", false),
        ];
        for (path, contents, linked) in paths {
            let (tx, rx) = crate::server::event_channel(32);
            let mut server = server(tx);
            server.config.workspace_dirs = vec![extra.display().to_string()];
            if path != ssh.join("config") {
                std::fs::write(ssh.join("config"), "Include included config.d/*\n").unwrap();
                if path == ssh.join("nested") {
                    std::fs::write(ssh.join("included"), "Include nested\n").unwrap();
                }
            }
            if linked {
                server.new_workspace("linked".into(), Some(path.clone()));
            }
            server.scan_definitions();
            until(&mut server, &rx, |s| s.jobs.is_empty());
            assert!(!server.config_scan_failed, "{}", path.display());
            let initial = server.next_id;
            server.check_definitions();
            assert_eq!(server.next_id, initial);
            std::fs::write(&path, contents).unwrap();
            one_scan(&mut server, &rx);
            std::fs::write(&path, format!("{contents}\n")).unwrap();
            one_scan(&mut server, &rx);
            let renamed = path.with_extension("renamed");
            std::fs::rename(&path, &renamed).unwrap();
            one_scan(&mut server, &rx);
            std::fs::rename(&renamed, &path).unwrap();
            one_scan(&mut server, &rx);
            std::fs::remove_file(&path).unwrap();
            one_scan(&mut server, &rx);
            let _ = std::fs::remove_file(ssh.join("config"));
            let _ = std::fs::remove_file(ssh.join("included"));
        }
    }

    #[test]
    fn directory_changes_and_symlink_targets_are_scanned() {
        if !crate::testing::in_own_process("server::reload::tests::directory_changes_and_symlink_targets_are_scanned") {
            return;
        }
        let home = crate::paths::home_dir().unwrap();
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("config"), "Include config.d/*\n").unwrap();
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        server.config.workspace_dirs = vec![home.join("extra").display().to_string()];
        server.scan_definitions();
        until(&mut server, &rx, |s| s.jobs.is_empty());
        for dir in [crate::paths::workspaces_dir(), home.join("extra"), ssh.join("config.d")] {
            std::fs::create_dir_all(&dir).unwrap();
            one_scan(&mut server, &rx);
            let renamed = dir.with_extension("renamed");
            std::fs::rename(&dir, &renamed).unwrap();
            one_scan(&mut server, &rx);
            std::fs::rename(&renamed, &dir).unwrap();
            one_scan(&mut server, &rx);
            std::fs::remove_dir(&dir).unwrap();
            one_scan(&mut server, &rx);
        }
        std::fs::create_dir_all(crate::paths::workspaces_dir()).unwrap();
        let target = home.join("target");
        std::fs::write(&target, "layout { pane; }\n").unwrap();
        let link = crate::paths::workspaces_dir().join("linked.kdl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        one_scan(&mut server, &rx);
        std::fs::write(&target, "layout { pane; pane; }\n").unwrap();
        one_scan(&mut server, &rx);
        std::fs::remove_file(&target).unwrap();
        one_scan(&mut server, &rx);
        std::fs::write(&target, "layout { pane; }\n").unwrap();
        one_scan(&mut server, &rx);
        // Unwalked directories and non-KDL contents are not scan inputs.
        std::fs::write(home.join("unrelated"), "changed").unwrap();
        let ignored = crate::paths::workspaces_dir().join("ignored.txt");
        std::fs::write(&ignored, "first").unwrap();
        one_scan(&mut server, &rx);
        std::fs::write(&ignored, "second").unwrap();
        let before = server.next_id;
        server.check_definitions();
        assert_eq!(server.next_id, before);
    }

    #[test]
    fn changes_during_a_scan_are_read_once_more_without_a_notification() {
        if !crate::testing::in_own_process(
            "server::reload::tests::changes_during_a_scan_are_read_once_more_without_a_notification",
        ) {
            return;
        }
        std::fs::create_dir_all(crate::paths::workspaces_dir()).unwrap();
        for workspace in [false, true] {
            let path = if workspace { crate::paths::workspaces_dir().join("held.kdl") } else { Accounts::path() };
            std::fs::write(&path, if workspace { "layout { pane; }" } else { "[limits]\nthreshold = 81\n" }).unwrap();
            let (tx, rx) = crate::server::event_channel(32);
            let mut server = server(tx);
            server.config.ssh_config_hosts = false;
            server.scan_definitions();
            until(&mut server, &rx, |s| s.jobs.is_empty());
            let (entered, reading) = crossbeam_channel::bounded(1);
            let (release, held) = crossbeam_channel::bounded(1);
            let first = server.next_id;
            crate::workspace::SOURCE_SCANS.set(0);
            crate::workspace::SOURCE_LOADS.set(0);
            server.start_inventory_scan(move |inputs| {
                let read = Inventories::read(false, inputs);
                entered.send(()).unwrap();
                held.recv_timeout(PATIENCE).unwrap();
                read
            });
            reading.recv_timeout(PATIENCE).unwrap();
            std::fs::write(&path, if workspace { "layout { pane; pane; }" } else { "[limits]\nthreshold = 82\n" })
                .unwrap();
            server.check_definitions();
            assert_eq!(server.next_id, first + 1);
            release.send(()).unwrap();
            until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
            assert_eq!(server.next_id, first + 2);
            assert_eq!(crate::workspace::SOURCE_SCANS.get(), 0, "workspace reads stay off the loop");
            assert_eq!(crate::workspace::SOURCE_LOADS.get(), 0);
            if !workspace {
                assert_eq!(server.accounts.limits.threshold, 82);
            }
            server.check_definitions();
            assert_eq!(server.next_id, first + 2);
        }
    }

    #[test]
    fn rename_during_a_scan_preserves_the_source_association() {
        if !crate::testing::in_own_process(
            "server::reload::tests::rename_during_a_scan_preserves_the_source_association",
        ) {
            return;
        }
        let dir = crate::paths::workspaces_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("a.kdl");
        let renamed = dir.join("b.kdl");
        let original = "layout { pane; }\n";
        std::fs::write(&path, original).unwrap();
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        server.config.ssh_config_hosts = false;
        server.new_workspace("linked".into(), Some(path.clone()));
        server.workspaces[0].definition_source = Some(original.into());
        server.scan_definitions();
        until(&mut server, &rx, |s| s.jobs.is_empty());
        let inode = server.workspaces[0].definition_inode.unwrap();
        let (entered, reading) = crossbeam_channel::bounded(1);
        let (release, held) = crossbeam_channel::bounded(1);
        let job = crate::server::jobs::JobId(server.next_id);
        server.start_inventory_scan(move |inputs| {
            let read = Inventories::read(false, inputs);
            entered.send(()).unwrap();
            held.recv_timeout(PATIENCE).unwrap();
            read
        });
        reading.recv_timeout(PATIENCE).unwrap();
        std::fs::rename(&path, &renamed).unwrap();
        release.send(()).unwrap();
        finish_job(&mut server, &rx, job);
        assert_eq!(server.workspaces[0].definition.as_ref(), Some(&path));
        assert_eq!(server.workspaces[0].definition_inode, Some(inode));
        assert!(server.config_scan.upgrade().is_some());
        until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
        assert_eq!(server.workspaces[0].definition.as_ref(), Some(&renamed));
        assert_eq!(server.workspaces[0].definition_inode, Some(inode));
        assert_eq!(server.workspaces[0].source_status, None);
        assert_eq!(server.def_files.len(), 1);
        assert_eq!(server.def_files[0].0, renamed);
        assert_eq!(server.cached_workspace_catalog().len(), 1, "the renamed source belongs to the kept workspace");
        let before = server.next_id;
        server.check_definitions();
        assert_eq!(server.next_id, before, "the fresh scan becomes the baseline");
    }

    #[test]
    fn idle_ticks_start_no_jobs_and_deliver_no_job_or_wake_events() {
        if !crate::testing::in_own_process(
            "server::reload::tests::idle_ticks_start_no_jobs_and_deliver_no_job_or_wake_events",
        ) {
            return;
        }
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        server.config.ssh_config_hosts = false;
        server.last_codex_poll = std::time::Instant::now();
        server.scan_definitions();
        until(&mut server, &rx, |s| s.jobs.is_empty());
        server.idle = true;
        let initial = server.next_id;
        let mut wakeups = 0;
        for tick in 1..=12 {
            let now = std::time::Instant::now() + std::time::Duration::from_secs(tick * 5);
            server.tick(now);
            assert!(server.jobs.is_empty());
            while let Ok(event) = rx.try_recv() {
                wakeups += usize::from(matches!(event, crate::server::Event::Job(..) | crate::server::Event::Wake));
                server.handle(event);
            }
        }
        assert_eq!(server.next_id, initial);
        assert_eq!(wakeups, 0, "12 idle ticks over 60 simulated seconds");
    }

    #[test]
    fn source_status_and_renames_use_the_background_results_in_open_pickers() {
        if !crate::testing::in_own_process(
            "server::reload::tests::source_status_and_renames_use_the_background_results_in_open_pickers",
        ) {
            return;
        }
        let dir = crate::paths::workspaces_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("source.kdl");
        let original = "layout { pane; }\n";
        std::fs::write(&path, original).unwrap();
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        let ws = server.clients[&id].workspace;
        server.workspaces[0].set_definition(Some(&path));
        server.workspaces[0].definition_source = Some(original.into());
        server.scan_definitions();
        until(&mut server, &rx, |s| s.jobs.is_empty());
        server.clients.get_mut(&id).unwrap().mode = crate::server::Mode::Picker(server.workspace_picker(ws));
        crate::workspace::SOURCE_SCANS.set(0);
        crate::workspace::SOURCE_LOADS.set(0);
        std::fs::write(&path, "layout { pane; pane; }\n").unwrap();
        one_scan(&mut server, &rx);
        assert_eq!(server.workspaces[0].source_status, Some("File changed"));
        let crate::server::Mode::Picker(picker) = &server.clients[&id].mode else { panic!("workspace picker") };
        assert!(picker.items.iter().any(|item| item.detail.contains("File changed")));
        let renamed = dir.join("renamed.kdl");
        std::fs::rename(&path, &renamed).unwrap();
        one_scan(&mut server, &rx);
        assert_eq!(server.workspaces[0].definition.as_ref(), Some(&renamed));
        assert_eq!(server.workspaces[0].source_status, Some("File changed"));
        std::fs::remove_file(&renamed).unwrap();
        one_scan(&mut server, &rx);
        assert_eq!(server.workspaces[0].source_status, Some("Source file missing"));
        assert_eq!(crate::workspace::SOURCE_SCANS.get(), 0);
        assert_eq!(crate::workspace::SOURCE_LOADS.get(), 0);
    }

    #[test]
    fn newly_linked_sources_are_tracked_and_unlinked_sources_are_forgotten() {
        if !crate::testing::in_own_process(
            "server::reload::tests::newly_linked_sources_are_tracked_and_unlinked_sources_are_forgotten",
        ) {
            return;
        }
        let path = crate::paths::home_dir().unwrap().join("external.kdl");
        std::fs::write(&path, "layout { pane; }\n").unwrap();
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        server.scan_definitions();
        until(&mut server, &rx, |s| s.jobs.is_empty());
        server.new_workspace("linked".into(), Some(path.clone()));
        one_scan(&mut server, &rx);
        std::fs::write(&path, "layout { pane; pane; }\n").unwrap();
        one_scan(&mut server, &rx);
        server.workspaces[0].set_definition(None);
        one_scan(&mut server, &rx);
        std::fs::write(&path, "layout { pane; }\n").unwrap();
        let before = server.next_id;
        server.check_definitions();
        assert_eq!(server.next_id, before);
    }

    #[test]
    fn disabled_ssh_hosts_do_not_add_scan_inputs() {
        if !crate::testing::in_own_process("server::reload::tests::disabled_ssh_hosts_do_not_add_scan_inputs") {
            return;
        }
        let ssh = crate::paths::home_dir().unwrap().join(".ssh");
        std::fs::create_dir_all(ssh.join("config.d")).unwrap();
        std::fs::write(ssh.join("config"), "Include config.d/*\n").unwrap();
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        server.config.ssh_config_hosts = false;
        server.scan_definitions();
        until(&mut server, &rx, |s| s.jobs.is_empty());
        std::fs::write(ssh.join("config"), "Host changed\n").unwrap();
        std::fs::write(ssh.join("config.d/new"), "Host added\n").unwrap();
        let before = server.next_id;
        server.check_definitions();
        assert_eq!(server.next_id, before);
    }

    #[test]
    fn failed_ssh_reads_recover_without_metadata_changes() {
        if !crate::testing::in_own_process("server::reload::tests::failed_ssh_reads_recover_without_metadata_changes") {
            return;
        }
        let ssh = crate::paths::home_dir().unwrap().join(".ssh");
        std::fs::create_dir_all(ssh.join("config.d")).unwrap();
        for synchronous in [false, true] {
            for source in ["config", "included", "config.d/host"] {
                std::fs::write(ssh.join("config"), "Include included config.d/*\n").unwrap();
                let path = ssh.join(source);
                let valid = "Host recovered\n";
                std::fs::write(&path, valid).unwrap();
                let (tx, rx) = crate::server::event_channel(32);
                let mut server = server(tx);
                server.scan_definitions();
                until(&mut server, &rx, |s| s.jobs.is_empty());
                assert!(!server.config_scan_failed);
                assert_eq!(server.known_servers.len(), 1);
                let mut invalid = valid.as_bytes().to_vec();
                invalid[0] = 0xff;
                std::fs::write(&path, invalid).unwrap();
                let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                for _ in 0..2 {
                    if synchronous {
                        server.scan_definitions();
                    } else {
                        let before = server.next_id;
                        server.check_definitions();
                        until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
                        assert_eq!(server.next_id, before + 1, "failed reads retry on a later tick");
                    }
                    assert!(server.config_scan_failed, "{source}");
                    assert!(server.known_servers.is_empty());
                    assert!(!server.config_inputs.as_ref().unwrap().changed().unwrap());
                }
                std::fs::write(&path, valid).unwrap();
                std::fs::File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
                assert!(!server.config_inputs.as_ref().unwrap().changed().unwrap(), "recovery preserves metadata");
                one_scan(&mut server, &rx);
                assert!(!server.config_scan_failed);
                assert_eq!(server.known_servers.len(), 1);
                assert_eq!(server.known_servers[0].server.name, "recovered");
                std::fs::remove_file(&path).unwrap();
            }
        }
    }

    #[test]
    fn failed_scans_and_explicit_requests_retry_without_metadata_changes() {
        if !crate::testing::in_own_process(
            "server::reload::tests::failed_scans_and_explicit_requests_retry_without_metadata_changes",
        ) {
            return;
        }
        std::fs::create_dir_all(crate::paths::config_dir()).unwrap();
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        server.config.ssh_config_hosts = false;
        server.scan_definitions();
        until(&mut server, &rx, |s| s.jobs.is_empty());
        server.config_scan_again = true;
        one_scan(&mut server, &rx);
        crate::server::jobs::FAIL_START.set(true);
        server.scan_definitions_async();
        assert!(server.config_scan.upgrade().is_none());
        assert!(server.config_scan_failed);
        one_scan(&mut server, &rx);
        server.start_inventory_scan(|_| panic!("failed scan"));
        until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
        assert!(server.config_scan_failed);
        one_scan(&mut server, &rx);
        std::os::unix::fs::symlink(Accounts::path(), Accounts::path()).unwrap();
        for _ in 0..2 {
            let before = server.next_id;
            server.check_definitions();
            until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
            assert_eq!(server.next_id, before + 1);
            assert!(server.config_scan_failed);
        }
        std::fs::remove_file(Accounts::path()).unwrap();
        one_scan(&mut server, &rx);
    }

    #[test]
    fn unchanged_invalid_contents_wait_for_a_change() {
        if !crate::testing::in_own_process("server::reload::tests::unchanged_invalid_contents_wait_for_a_change") {
            return;
        }
        std::fs::create_dir_all(crate::paths::workspaces_dir()).unwrap();
        let kdl = crate::paths::workspaces_dir().join("broken.kdl");
        for (path, valid, applied) in
            [(kdl, "layout { pane; }", (1, 90)), (Accounts::path(), "[limits]\nthreshold = 81\n", (0, 81))]
        {
            std::fs::write(&path, "layout {").unwrap();
            let (tx, rx) = crate::server::event_channel(32);
            let mut server = server(tx);
            server.config.ssh_config_hosts = false;
            let (id, _peer) = client(&mut server);
            server.scan_definitions();
            until(&mut server, &rx, |s| s.jobs.is_empty());
            assert!(!server.config_scan_failed, "{}", path.display());
            let before = server.next_id;
            server.check_definitions();
            assert_eq!(server.next_id, before, "a parse error cannot go away without a change");
            server.clients.get_mut(&id).unwrap().dirty = crate::server::Dirty::No;
            server.config_scan_again = true;
            one_scan(&mut server, &rx);
            assert_eq!(server.clients[&id].dirty, crate::server::Dirty::No, "nothing changed");
            std::fs::write(&path, valid).unwrap();
            let before = server.next_id;
            server.check_definitions();
            // Valid accounts also start their own checks: count the scan alone.
            assert_eq!(server.next_id, before + 1, "the fixed file is read");
            until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
            assert_eq!((server.def_files.len(), server.accounts.limits.threshold), applied);
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn reload_reads_off_the_loop_and_only_the_newest_request_applies() {
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        let (first, _peer) = client(&mut server);
        let first_reply = server.clients.get_mut(&first).unwrap().out.capture();
        let (entered, reading) = crossbeam_channel::bounded(1);
        let (release, held) = crossbeam_channel::bounded(1);
        let owner = std::thread::current().id();
        assert!(matches!(
            server.start_config_reload(ReplyHandle::new(first), move || {
                assert_ne!(std::thread::current().id(), owner);
                entered.send(()).unwrap();
                held.recv_timeout(PATIENCE).unwrap();
                Config::parse("scrollback = 123\n")
            }),
            RequestOutcome::Deferred
        ));
        reading.recv_timeout(PATIENCE).unwrap();
        assert!(matches!(server.request(0, Request::Ping).immediate(), Ok(Response::Pong { .. })));
        assert_eq!(server.config.scrollback, Config::default().scrollback);
        assert!(first_reply.is_empty());

        let (second, _peer) = client(&mut server);
        let second_reply = server.clients.get_mut(&second).unwrap().out.capture();
        assert!(matches!(
            server
                .start_config_reload(ReplyHandle::new(second), || Config::parse("scrollback = 456\nfuture_key = 1\n")),
            RequestOutcome::Deferred
        ));
        until(&mut server, &rx, |s| !s.clients.contains_key(&second));
        let Response::Text(warnings) = response(&second_reply).unwrap() else { panic!("reload warnings") };
        assert!(warnings.contains("future_key: unknown key"), "{warnings}");
        assert_eq!(server.config.scrollback, 456);
        release.send(()).unwrap();
        until(&mut server, &rx, |s| !s.clients.contains_key(&first));
        assert!(response(&first_reply).unwrap_err().contains("newer config reload"));
        assert_eq!(server.config.scrollback, 456);
    }

    #[test]
    fn reload_refuses_special_files_without_changing_the_configuration() {
        if !crate::testing::in_own_process(
            "server::reload::tests::reload_refuses_special_files_without_changing_the_configuration",
        ) {
            return;
        }
        let path = crate::paths::config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for fifo in [true, false] {
            if fifo {
                let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: the path is terminated and belongs to this isolated test.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            } else {
                std::os::unix::fs::symlink("/dev/null", &path).unwrap();
            }
            let (tx, rx) = crate::server::event_channel(32);
            let mut server = server(tx);
            let (id, _peer) = client(&mut server);
            let reply = server.clients.get_mut(&id).unwrap().out.capture();
            server.config.scrollback = 321;
            assert!(matches!(server.request(id, Request::ReloadConfig), RequestOutcome::Deferred));
            until(&mut server, &rx, |s| !s.clients.contains_key(&id));
            let error = response(&reply).unwrap_err();
            assert!(error.starts_with("config: reading "), "{error}");
            assert!(error.contains("not a regular file"), "{error}");
            assert_eq!(server.config.scrollback, 321);
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn inventory_scan_coalesces_reads_and_discards_results_overtaken_by_edits() {
        if !crate::testing::in_own_process(
            "server::reload::tests::inventory_scan_coalesces_reads_and_discards_results_overtaken_by_edits",
        ) {
            return;
        }
        std::fs::create_dir_all(crate::paths::config_dir()).unwrap();
        for changed in ["accounts", "servers", "config"] {
            let (tx, rx) = crate::server::event_channel(32);
            let mut server = server(tx);
            server.config.ssh_config_hosts = false;
            std::fs::write(Accounts::path(), "[limits]\nthreshold = 81\n").unwrap();
            std::fs::write(Inventory::path(), "[[server]]\nname = 'fresh'\nhost = 'fresh.test'\n").unwrap();
            let (entered, reading) = crossbeam_channel::bounded(1);
            let (release, held) = crossbeam_channel::bounded(1);
            let owner = std::thread::current().id();
            let job = crate::server::jobs::JobId(server.next_id);
            server.start_inventory_scan(move |_| {
                assert_ne!(std::thread::current().id(), owner);
                let mut accounts = Accounts::empty();
                accounts.limits.threshold = 12;
                entered.send(()).unwrap();
                held.recv_timeout(PATIENCE).unwrap();
                Inventories {
                    accounts: Ok(accounts),
                    servers: Ok(Inventory::empty()),
                    hosts: Vec::new(),
                    hosts_failed: false,
                }
            });
            reading.recv_timeout(PATIENCE).unwrap();
            server.start_inventory_scan(|_| panic!("a scan is already reading"));
            assert!(matches!(server.request(0, Request::Ping).immediate(), Ok(Response::Pong { .. })));
            match changed {
                "accounts" => server.reload_accounts(),
                "servers" => server.refresh_known(&[]),
                _ => {
                    server.apply_config(Config { ssh_config_hosts: false, ..Config::default() }, Vec::new());
                }
            }
            let threshold = server.accounts.limits.threshold;
            release.send(()).unwrap();
            // The first completion must discard its stale data before starting another read.
            finish_job(&mut server, &rx, job);
            assert_eq!(server.accounts.limits.threshold, threshold);
            assert!(server.config_scan.upgrade().is_some());
            until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
            assert_eq!(server.accounts.limits.threshold, 81);
            assert!(server.known_servers.iter().any(|s| s.server.name == "fresh"));
        }
    }

    #[test]
    fn usage_notifications_do_not_starve_a_held_inventory_scan() {
        if !crate::testing::in_own_process(
            "server::reload::tests::usage_notifications_do_not_starve_a_held_inventory_scan",
        ) {
            return;
        }
        std::fs::create_dir_all(crate::paths::config_dir()).unwrap();
        std::fs::write(Accounts::path(), "[limits]\nthreshold = 81\n").unwrap();
        std::fs::write(Inventory::path(), "[[server]]\nname = 'fresh'\nhost = 'fresh.test'\n").unwrap();
        let usage = crate::paths::state_dir().join("usage/by-id");
        std::fs::create_dir_all(&usage).unwrap();
        let (changed, changes) = crossbeam_channel::unbounded();
        let watcher = crate::watch::Watcher::new(move |events| {
            let _ = changed.send(events);
        })
        .unwrap();
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        server.config.ssh_config_hosts = false;
        server.dirwatch = super::super::dirwatch::DirWatch::new(Some(watcher));
        server.dirwatch.watch_config(std::slice::from_ref(&usage));
        let (entered, reading) = crossbeam_channel::bounded(1);
        let (release, held) = crossbeam_channel::bounded(1);
        let job = crate::server::jobs::JobId(server.next_id);
        server.start_inventory_scan(move |_| {
            let read = Inventories {
                accounts: Accounts::load(),
                servers: Inventory::load(),
                hosts: Vec::new(),
                hosts_failed: false,
            };
            entered.send(()).unwrap();
            held.recv_timeout(PATIENCE).unwrap();
            read
        });
        reading.recv_timeout(PATIENCE).unwrap();
        // Capture the usage watch's notification once, then deliver it repeatedly
        // without depending on the kernel's batching or the reader's scheduling.
        std::fs::write(usage.join("changed"), "").unwrap();
        let changes = changes.recv_timeout(PATIENCE).unwrap();
        let generation = server.config_generation;
        for _ in 0..8 {
            server.handle(crate::server::Event::Watched(changes.clone()));
        }
        assert!(server.config_scan_again);
        assert_eq!(server.jobs.len(), 1, "notifications coalesce into one pending rescan");
        assert_eq!(server.config_generation, generation);
        std::fs::write(Accounts::path(), "[limits]\nthreshold = 82\n").unwrap();
        release.send(()).unwrap();
        finish_job(&mut server, &rx, job);
        assert_eq!(server.accounts.limits.threshold, 81, "the held result applies despite usage events");
        assert!(server.known_servers.iter().any(|s| s.server.name == "fresh"));
        assert!(server.config_scan.upgrade().is_some(), "the pending rescan starts");
        let job = crate::server::jobs::JobId(server.next_id - 1);
        for _ in 0..8 {
            server.handle(crate::server::Event::Watched(changes.clone()));
        }
        finish_job(&mut server, &rx, job);
        assert_eq!(server.accounts.limits.threshold, 82, "the next result also applies during usage events");
        until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
        assert!(!server.config_scan_again);
    }

    #[test]
    fn automatic_inventory_reads_report_special_files_and_recover() {
        if !crate::testing::in_own_process(
            "server::reload::tests::automatic_inventory_reads_report_special_files_and_recover",
        ) {
            return;
        }
        std::fs::create_dir_all(crate::paths::config_dir()).unwrap();
        let accounts = Accounts::path();
        let inventory = Inventory::path();
        let name = std::ffi::CString::new(accounts.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the path is terminated and belongs to this isolated test.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink("/dev/null", &inventory).unwrap();
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        server.config.ssh_config_hosts = false;
        server.accounts.limits.threshold = 62;
        for _ in 0..2 {
            server.check_definitions();
            assert!(matches!(server.request(0, Request::Ping).immediate(), Ok(Response::Pong { .. })));
            until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
            assert_eq!(server.accounts.limits.threshold, 62);
            assert!(server.accounts_error.as_ref().unwrap().contains("not a regular file"));
            assert!(server.known_servers.is_empty());
        }
        assert_eq!(server.warnings.iter().filter(|w| w.contains("accounts.toml")).count(), 1);
        assert!(server.warnings.iter().any(|w| w.contains("servers.toml") && w.contains("not a regular file")));
        std::fs::remove_file(&accounts).unwrap();
        std::fs::remove_file(&inventory).unwrap();
        std::fs::write(accounts, "[limits]\nthreshold = 81\n").unwrap();
        std::fs::write(inventory, "[[server]]\nname = 'fresh'\nhost = 'fresh.test'\n").unwrap();
        server.check_definitions();
        until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
        assert!(server.accounts_error.is_none());
        assert_eq!(server.accounts.limits.threshold, 81);
        assert!(server.known_servers.iter().any(|s| s.server.name == "fresh"));
    }

    #[test]
    fn failed_worker_start_leaves_reload_answerable_and_scan_retryable() {
        let (tx, rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        let reply = server.clients.get_mut(&id).unwrap().out.capture();
        crate::server::jobs::FAIL_START.set(true);
        let RequestOutcome::Immediate { reply: handle, result } = server.request(id, Request::ReloadConfig) else {
            panic!("a failed start must be answered");
        };
        handle.complete(&mut server, result);
        assert!(response(&reply).unwrap_err().contains("cannot start config reload"));
        assert!(!server.clients.contains_key(&id));
        crate::server::jobs::FAIL_START.set(true);
        server.start_inventory_scan(|_| panic!("worker must not start"));
        assert!(server.config_scan.upgrade().is_none());
        server.start_inventory_scan(|_| Inventories {
            accounts: Err(anyhow::anyhow!("unreadable accounts")),
            servers: Err(anyhow::anyhow!("unreadable servers")),
            hosts: Vec::new(),
            hosts_failed: false,
        });
        until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
        assert_eq!(server.accounts_error.as_deref(), Some("unreadable accounts"));
        assert!(server.warnings.iter().any(|w| w == "unreadable servers"));
    }
}
