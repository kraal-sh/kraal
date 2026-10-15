//! `kraal config reload`: read config.toml again while the server runs.

use super::requests::{ReplyHandle, RequestOutcome};
use super::{Server, events, frame_bytes};
use crate::accounts::Accounts;
use crate::config::Config;
use crate::keymap::Keymap;
use crate::protocol::Response;
use crate::servers::Inventory;
use crate::workspace::WorkspaceDef;

struct Inventories {
    accounts: anyhow::Result<Accounts>,
    servers: anyhow::Result<Inventory>,
    hosts: Vec<String>,
}

impl Server {
    pub(super) fn scan_inventories(&mut self, defs: Vec<WorkspaceDef>) {
        let ssh_hosts = self.config.ssh_config_hosts;
        self.start_inventory_scan(defs, move || Inventories {
            accounts: Accounts::load(),
            servers: Inventory::load(),
            hosts: if ssh_hosts { super::ssh_config_hosts() } else { Vec::new() },
        });
    }

    fn start_inventory_scan(&mut self, defs: Vec<WorkspaceDef>, read: impl FnOnce() -> Inventories + Send + 'static) {
        if self.config_scan.upgrade().is_some() {
            self.config_scan_again = true;
            return;
        }
        self.config_scan_again = false;
        let pending = std::rc::Rc::new(());
        self.config_scan = std::rc::Rc::downgrade(&pending);
        let generation = self.config_generation;
        self.background(read, move |server, read| {
            drop(pending);
            // Only applied edits invalidate reads. Requests for another scan must not
            // starve completed reads while usage notifications keep arriving.
            let stale = generation != server.config_generation;
            if !stale {
                server.apply_known(&defs, read.servers, &read.hosts);
                server.apply_accounts(read.accounts);
                server.mark_dirty();
            }
            if std::mem::take(&mut server.config_scan_again) || stale {
                server.scan_definitions_async();
            }
        });
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
            server.start_inventory_scan(Vec::new(), move || {
                assert_ne!(std::thread::current().id(), owner);
                let mut accounts = Accounts::empty();
                accounts.limits.threshold = 12;
                entered.send(()).unwrap();
                held.recv_timeout(PATIENCE).unwrap();
                Inventories { accounts: Ok(accounts), servers: Ok(Inventory::empty()), hosts: Vec::new() }
            });
            reading.recv_timeout(PATIENCE).unwrap();
            server.start_inventory_scan(Vec::new(), || panic!("a scan is already reading"));
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
        server.start_inventory_scan(Vec::new(), move || {
            let read = Inventories { accounts: Accounts::load(), servers: Inventory::load(), hosts: Vec::new() };
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
            server.scan_definitions_async();
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
        server.scan_definitions_async();
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
        server.start_inventory_scan(Vec::new(), || panic!("worker must not start"));
        assert!(server.config_scan.upgrade().is_none());
        server.start_inventory_scan(Vec::new(), || Inventories {
            accounts: Err(anyhow::anyhow!("unreadable accounts")),
            servers: Err(anyhow::anyhow!("unreadable servers")),
            hosts: Vec::new(),
        });
        until(&mut server, &rx, |s| s.config_scan.upgrade().is_none());
        assert_eq!(server.accounts_error.as_deref(), Some("unreadable accounts"));
        assert!(server.warnings.iter().any(|w| w == "unreadable servers"));
    }
}
