use std::time::Duration;

use super::super::Server;
use super::super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::choices::switch_effect;
use super::discovery::{PROCESS_UNVERIFIED, observed_command};
use super::switching::switch_timing;
use super::*;
use crate::account_status;
use crate::accounts::{self, Accounts, Mode as LimitMode};
use crate::agent::AgentState;
use crate::protocol::Placement;
use crate::theme::NoticeKind;

#[test]
fn launch_picker_lists_only_installed_registry_entries() {
    let mut t = SwitchTest::new("registry-picker");
    let bin = t.dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    assert!(t.server.agent_picker_on_path(99, None, bin.as_os_str()).items.is_empty());
    crate::testing::write_script(&bin.join("gemini"), "#!/bin/sh\nexit 99\n");
    let picker = t.server.agent_picker_on_path(99, None, bin.as_os_str());
    assert_eq!(picker.items.len(), 1);
    assert_eq!(picker.items[0].label, "Gemini");
    assert_eq!(picker.items[0].detail, "Accounts are managed in Gemini");
    assert!(matches!(&picker.items[0].action, PickAction::AgentLaunch { agent, account: None } if agent == "gemini"));
    assert!(!picker.items.iter().any(|i| i.label == "Aider"));

    let mut codex = t.server.accounts.get("claude", "from").unwrap();
    codex.id = crate::identity::new_uuid();
    codex.agent = "codex".into();
    codex.home = t.dir.join("codex").display().to_string();
    t.server.accounts.add(codex).unwrap();
    for agent in crate::agent_registry::AGENTS {
        let name = agent.launches[0][0];
        let runtime = agent.scripts.iter().find(|(s, _)| *s == name).map_or("sh", |(_, runtime)| *runtime);
        crate::testing::write_script(&bin.join(name), &format!("#!/usr/bin/env {runtime}\n"));
    }
    let picker = t.server.agent_picker_on_path(99, None, bin.as_os_str());
    let listed: Vec<_> = picker
        .items
        .iter()
        .map(|i| match &i.action {
            PickAction::AgentLaunch { agent, .. } => agent.as_str(),
            _ => panic!("only launch destinations"),
        })
        .collect();
    assert_eq!(&listed[..3], ["claude", "claude", "codex"]);
    assert_eq!(
        listed.into_iter().collect::<std::collections::BTreeSet<_>>(),
        crate::agent_registry::AGENTS.iter().map(|a| a.name).collect()
    );
}

#[test]
fn launch_picker_rejects_python_letta_and_offers_node_letta() {
    let mut t = SwitchTest::new("letta-runtime");
    let bin = t.dir.join("bin");
    let later = t.dir.join("later");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&later).unwrap();
    crate::testing::write_script(&bin.join("letta"), "#!/usr/bin/python3\nraise SystemExit(99)\n");
    crate::testing::write_script(&later.join("letta"), "#!/usr/bin/env node\nprocess.exit(99);\n");
    let path = std::env::join_paths([&bin, &later]).unwrap();
    assert!(
        t.server.agent_picker_on_path(99, None, &path).items.is_empty(),
        "the Python launcher shadows the later Node launcher"
    );
    let package = t.dir.join("index.js");
    crate::testing::write_script(&package, "#!/usr/bin/env node\nprocess.exit(99);\n");
    std::fs::remove_file(bin.join("letta")).unwrap();
    std::os::unix::fs::symlink(package, bin.join("letta")).unwrap();
    let picker = t.server.agent_picker_on_path(99, None, &path);
    assert_eq!(picker.items.len(), 1);
    assert!(matches!(&picker.items[0].action,
            PickAction::AgentLaunch { agent, account: None } if agent == "letta"));
}

#[test]
fn relative_path_agents_use_the_focused_panes_directory() {
    if !crate::testing::in_own_process("server::agents::tests::relative_path_agents_use_the_focused_panes_directory") {
        return;
    }
    let mut t = SwitchTest::new("relative-agent-path");
    let server_dir = t.dir.join("home");
    let repo = t.dir.join("repo");
    std::fs::create_dir(&server_dir).unwrap();
    std::fs::create_dir_all(repo.join(".venv/bin")).unwrap();
    crate::testing::write_script(&repo.join(".venv/bin/aider"), "#!/bin/sh\nexit 0\n");
    let (client, _peer) = super::super::tests::client(&mut t.server);
    let ws = t.server.clients[&client].workspace;
    let tab = &mut t.server.workspaces.last_mut().unwrap().tabs[0];
    tab.root = Some(crate::layout::Node::Leaf(1));
    tab.set_focus(1);
    t.server.panes.get_mut(&1).unwrap().reported_cwd = Some(repo.clone());
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(&server_dir).unwrap();
    let path = ".venv/bin:/usr/bin";
    // This test has its own process, with no concurrent tests reading PATH.
    unsafe { std::env::set_var("PATH", path) };
    let registered = crate::agent_registry::find("aider").unwrap();
    assert!(registered.command_on_path(path.as_ref(), &server_dir).is_none());
    let picker = t.server.agent_picker(client, None);
    assert!(picker.items.iter().any(|item| matches!(&item.action,
            PickAction::AgentLaunch { agent, account: None } if agent == "aider")));
    let pane = t.server.launch_agent(ws, Some(1), "aider", None, Placement::Tab, None).unwrap();
    assert_eq!(t.server.panes[&pane].spec.command, ["aider"]);
    assert_eq!(t.server.panes[&pane].spec.cwd.as_ref(), Some(&repo));
    std::env::set_current_dir(previous).unwrap();
}

#[test]
fn native_account_actions_explain_the_limit_without_offering_a_switch() {
    let mut t = SwitchTest::new("native-account");
    t.server.switches.clear();
    let (client, _peer) = super::super::tests::client(&mut t.server);
    let pane = t.server.panes.get_mut(&1).unwrap();
    pane.spec.account = None;
    pane.spec.account_id = None;
    pane.agent.kind = Some("gemini".into());
    let before = pane.spec.clone();
    assert!(matches!(t.server.change_account_mode(client, 1), Mode::Normal));
    assert_eq!(t.server.clients[&client].toast.as_ref().unwrap().0, "Accounts are managed in Gemini");
    assert_eq!(t.server.panes[&1].spec.command, before.command);
    assert!(t.server.switches.is_empty());

    let picker = Picker::new(
        PickerKind::Agent,
        "Start an agent",
        vec![PickItem::new(
            "Gemini",
            "Accounts are managed in Gemini",
            PickAction::AgentLaunch { agent: "gemini".into(), account: None },
        )],
    );
    t.server.clients.get_mut(&client).unwrap().mode = Mode::Picker(picker);
    t.server.client_input(client, b"\x04");
    let Mode::Picker(picker) = &t.server.clients[&client].mode else { panic!("keep the launch list") };
    assert_eq!(picker.message.as_deref(), Some("Accounts are managed in Gemini"));

    t.server.clients.get_mut(&client).unwrap().mode = Mode::Normal;
    let tab = &mut t.server.workspaces.last_mut().unwrap().tabs[0];
    tab.root = Some(crate::layout::Node::Leaf(1));
    tab.set_focus(1);
    t.server.client_input(client, b"\x02An");
    assert!(matches!(t.server.clients[&client].mode, Mode::Normal));
    assert_eq!(t.server.clients[&client].toast.as_ref().unwrap().0, "Accounts are managed in Gemini");
    assert!(t.server.accounts.for_agent("gemini").is_empty());
}

#[test]
fn finishing_discovery_keeps_the_clicked_account_in_place() {
    for (cols, rows) in [(80, 24), (124, 36)] {
        for finished in [&["claude"][..], &["codex"], &["claude", "codex"], &["codex", "claude"]] {
            let (tx, _) = super::super::queue::channel(16);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            c.cols = cols;
            c.rows = rows;
            let [_, second] = ["work", "second"].map(|name| {
                server
                    .accounts
                    .add(Account {
                        id: crate::identity::new_uuid(),
                        agent: "codex".into(),
                        name: name.into(),
                        home: format!("/nonexistent/{name}"),
                        threshold: None,
                        note: None,
                        sign_in_required: false,
                        login_revision: String::new(),
                        review: false,
                        public_identity: None,
                        linked_to: None,
                        references: Vec::new(),
                    })
                    .unwrap()
            });
            for agent in ["claude", "codex"] {
                server.existing_checks.insert(agent.into(), ExistingCheck::Checking);
            }
            let picker = server.accounts_picker(None);
            let layout = super::super::overlays::picker_layout(&picker, server.client_screen(client));
            let mut buffer = ratatui::buffer::Buffer::empty(server.client_screen(client));
            super::super::overlays::draw_picker(&picker, buffer.area, &mut buffer);
            let (col, row) = (layout.list.x + 2, layout.list.y + 2);
            let label: String = (col..col + 14).map(|x| buffer[(x, row)].symbol()).collect();
            assert_eq!(label, "Codex · second");
            server.clients.get_mut(&client).unwrap().mode = Mode::Picker(picker);
            // The user clicks the row they saw while the background checks finish.
            for &agent in finished {
                server.existing_checks.insert(agent.into(), ExistingCheck::Checked(Status::NotSignedIn));
                server.refresh_agent_pickers();
            }
            server.client_input(client, format!("\x1b[<0;{};{}M", col + 1, row + 1).as_bytes());
            let Mode::AccountDetails(details) = &server.clients[&client].mode else {
                panic!("the click did not open details");
            };
            assert_eq!(details.account, second.id, "{cols}x{rows}: the clicked row changed accounts");
            server.client_input(client, b"\t\r");
            let Mode::Prompt(prompt) = &server.clients[&client].mode else {
                panic!("Rename did not open");
            };
            assert_eq!(prompt.title, "Rename Codex · second");
        }
    }
}

#[test]
fn account_discovery_starts_each_check_once_and_handles_results_without_a_tick() {
    use super::super::{Event, queue::Wait};

    for reverse in [false, true] {
        let (tx, rx) = super::super::queue::channel(16);
        let mut server = super::super::tests::server(tx);
        server.check_existing_accounts();
        assert_eq!(server.jobs.len(), accounts::agents().count());
        assert!(server.existing_checks.values().all(|s| *s == ExistingCheck::Checking));
        server.refresh_existing_accounts();
        assert_eq!(server.jobs.len(), accounts::agents().count(), "opening Accounts shares the pending checks");

        // The isolated PATH has neither agent. Keep their completed jobs, not a status
        // subprocess racing its watchdog, and deliver them in each order.
        let mut results = Vec::new();
        for _ in accounts::agents() {
            let Wait::Event(Event::Job(id, result)) = rx.wait(crate::testing::PATIENCE) else {
                panic!("the check did not publish its result");
            };
            results.push((id, result));
        }
        results.sort_by_key(|(id, _)| *id);
        if reverse {
            results.reverse();
        }
        let (client, _peer) = super::super::tests::client(&mut server);
        server.clients.get_mut(&client).unwrap().mode = Mode::Picker(server.accounts_picker(None));
        let last_tick = server.last_tick;
        for (i, (id, result)) in results.into_iter().enumerate() {
            server.handle(Event::Job(id, result));
            assert_eq!(server.jobs.len(), accounts::agents().count() - i - 1);
            assert_eq!(
                server.existing_checks.values().filter(|s| **s == ExistingCheck::Checked(Status::NotInstalled)).count(),
                i + 1,
                "each result is published while the other is still pending"
            );
            let Mode::Picker(picker) = &server.clients[&client].mode else { panic!("Accounts stays open") };
            for (agent, state) in &server.existing_checks {
                let item = picker.items.iter().find(|item| item.label == accounts::existing_label(agent));
                if *state == ExistingCheck::Checking {
                    assert_eq!(item.unwrap().detail, "Checking sign-in");
                } else {
                    assert!(item.is_none(), "a completed missing agent no longer shows as checking");
                }
            }
            if i == 0 {
                server.handle(Event::Warning("server-responsive".into()));
                assert_eq!(server.warnings, ["server-responsive"]);
            }
        }
        assert!(server.jobs.is_empty());
        assert!(server.existing_checks.values().all(|s| *s == ExistingCheck::Checked(Status::NotInstalled)));
        assert_eq!(server.last_tick, last_tick);
        assert!(server.panes.is_empty(), "discovery never starts a login");
    }
}

// These tests run in their own processes because discovery reads the agent from PATH.
fn identity_discovery(name: &str) -> (SwitchTest, Account, PathBuf) {
    let mut t = SwitchTest::new(name);
    t.server.switches.clear();
    t.server.panes.clear();
    t.server.accounts.remove("claude", "to");
    t.server.accounts.rename("claude", "from", "work").unwrap();
    let work = t.server.accounts.get("claude", "work").unwrap();
    t.server.accounts.save().unwrap();
    t.server.existing_checks.insert("codex".into(), ExistingCheck::Checked(Status::NotInstalled));
    let standard = dirs::home_dir().unwrap().join(".claude");
    std::fs::create_dir_all(&standard).unwrap();
    let bin = t.dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    crate::testing::write_script(
        &bin.join("claude"),
        r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
case "$*" in
    'auth status')
        echo check >> "$home/checks"
        test ! -f "$home/identity" || cat "$home/identity"
        exit "$(cat "$home/status" 2>/dev/null || echo 1)"
        ;;
    'auth login') exec sleep 100 ;;
    *) exit 2 ;;
esac
"#,
    );
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())))
            .unwrap();
    // SAFETY: this test runs alone in its own process, before any checks start.
    unsafe { std::env::set_var("PATH", path) };
    (t, work, standard)
}

fn signed_in(home: &std::path::Path, email: &str) {
    std::fs::write(home.join("status"), "0").unwrap();
    std::fs::write(home.join("identity"), format!(r#"{{"loggedIn":true,"email":"{email}"}}"#)).unwrap();
}

#[test]
fn automatic_join_leaves_sources_unchanged_and_keeps_the_notice_until_attachment() {
    use crate::paths;

    if !crate::testing::in_own_process(
        "server::agents::tests::automatic_join_leaves_sources_unchanged_and_keeps_the_notice_until_attachment",
    ) {
        return;
    }
    for edited in [false, true] {
        let (mut t, work, standard) = identity_discovery(if edited { "join-edited-source" } else { "join-source" });
        signed_in(std::path::Path::new(&work.home), "a@example.com");
        signed_in(&standard, "a@example.com");
        let original = "// My layout.\nlayout { pane agent=\"claude:default\"; }\n";
        let source = if edited { format!("{original}// Outside edit before joining.\n") } else { original.into() };
        let target = t.dir.join("source.kdl");
        std::fs::write(&target, &source).unwrap();
        std::fs::create_dir_all(paths::workspaces_dir()).unwrap();
        let path = paths::workspaces_dir().join("joined.kdl");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        t.server.new_workspace("w".into(), Some(path.clone()));
        t.server.workspaces[0].definition_source = Some(original.into());

        t.server.check_existing_accounts();
        t.until("the account joins", |t| t.server.jobs.is_empty());
        assert_eq!(t.server.accounts.get("claude", "default").unwrap().id, work.id);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), source);
        let baseline = original;
        assert_eq!(t.server.workspaces[0].definition_source.as_deref(), Some(baseline));
        t.server.refresh_source_status();
        assert_eq!(t.server.workspaces[0].source_status, edited.then_some("File changed"));
        let snapshot = crate::server::persistence::snapshot(&t.server);
        assert_eq!(snapshot.workspaces[0].definition_source.as_deref(), Some(baseline));

        let notice = "~/.claude is Claude · work";
        t.server.show_warnings();
        assert_eq!(t.server.account_notices, [notice]);
        assert_eq!(t.server.messages.iter().filter(|m| m.text == notice).count(), 1);
        let (client, _peer) = super::super::tests::client(&mut t.server);
        t.server.clients.get_mut(&client).unwrap().mode = Mode::Picker(t.server.accounts_picker(None));
        t.server.show_warnings();
        let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 124, 36));
        t.server.compose(client, 1_000, &mut buffer);
        let row: String = (0..124).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(row.contains(notice), "the Accounts overlay keeps the notice visible: {row}");
        assert!(t.server.account_notices.is_empty());
        // Expiring or replacing the toast cannot lose the one message-log entry.
        t.server.clients.get_mut(&client).unwrap().toast = None;
        t.server.show_warnings();
        assert!(t.server.clients[&client].toast.is_none());
        assert_eq!(t.server.messages.iter().filter(|m| m.text == notice).count(), 1);

        std::fs::write(&target, format!("{source}// Outside edit after joining.\n")).unwrap();
        t.server.refresh_source_status();
        assert_eq!(t.server.workspaces[0].source_status, Some("File changed"));
        assert_eq!(t.server.workspaces[0].definition_source.as_deref(), Some(baseline));
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn discovery_saves_off_the_event_loop_while_the_accounts_lock_is_held() {
    use super::super::{Event, queue::Wait};

    if !crate::testing::in_own_process(
        "server::agents::tests::discovery_saves_off_the_event_loop_while_the_accounts_lock_is_held",
    ) {
        return;
    }
    let (mut t, work, standard) = identity_discovery("discovery-locked");
    // Opening Accounts shares this unrelated pending check.
    t.server.existing_checks.insert("codex".into(), ExistingCheck::Checking);
    signed_in(std::path::Path::new(&work.home), "a@example.com");
    signed_in(&standard, "a@example.com");
    t.server.check_existing_accounts();
    let Wait::Event(event @ Event::Job(..)) = t.rx.wait(crate::testing::PATIENCE) else {
        panic!("discovery did not finish");
    };
    let (client, _peer) = super::super::tests::client(&mut t.server);
    crate::paths::locked(&Accounts::path(), || {
        let start = Instant::now();
        t.server.handle(event);
        t.server.handle(Event::Warning("server-responsive".into()));
        assert_eq!(t.server.warnings, ["server-responsive"]);
        t.server.client_input(client, b"\x02Al");
        assert!(matches!(&t.server.clients[&client].mode, Mode::Picker(p) if p.kind == PickerKind::Accounts));
        let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 124, 36));
        t.server.compose(client, 1_000, &mut buffer);
        assert!(crate::testing::buffer_text(&buffer).contains("Checking sign-in"));
        assert!(start.elapsed() < Duration::from_millis(500), "the event loop waited for the accounts lock");
        assert_eq!(t.server.jobs.len(), 1, "the save stays on its worker until the lock is released");
        assert_eq!(Accounts::load().unwrap().get("claude", "default").unwrap().name, "default");
    })
    .unwrap();
    t.until("the unlocked save finishes", |t| t.server.jobs.is_empty());
    assert_eq!(t.server.accounts.get("claude", "default").unwrap().id, work.id);
    assert_eq!(t.server.account_notices, ["~/.claude is Claude · work"]);
}

#[test]
fn discovery_without_a_verified_identity_does_not_check_named_homes() {
    if !crate::testing::in_own_process(
        "server::agents::tests::discovery_without_a_verified_identity_does_not_check_named_homes",
    ) {
        return;
    }
    let (mut t, work, standard) = identity_discovery("discovery-no-identity");
    signed_in(std::path::Path::new(&work.home), "a@example.com");
    for status in ["1", "2", "0"] {
        std::fs::write(standard.join("status"), status).unwrap();
        t.server.existing_checks.remove("claude");
        t.server.check_existing_accounts();
        t.until("discovery finishes", |t| t.server.jobs.is_empty());
        assert!(!std::path::Path::new(&work.home).join("checks").exists());
        assert!(t.server.account_identities.is_empty());
    }
}

#[test]
fn login_invalidates_cached_and_in_flight_identity_results() {
    use super::super::{Event, queue::Wait};

    if !crate::testing::in_own_process("server::agents::tests::login_invalidates_cached_and_in_flight_identity_results")
    {
        return;
    }
    for in_flight in [false, true] {
        let (mut t, work, standard) = identity_discovery(if in_flight { "identity-late" } else { "identity-cached" });
        let home = std::path::Path::new(&work.home);
        signed_in(home, "a@example.com");
        std::fs::write(standard.join("status"), "1").unwrap();
        t.server.check_existing_accounts();
        t.until("unsigned home discovery finishes", |t| t.server.jobs.is_empty());
        // An identity observed before login must not survive the account fields returning
        // to their original values, even when its discovery result arrives after login.
        let checked = account_status::inspect(&work);
        t.server.account_identities.insert(work.id.clone(), (work.clone(), checked));
        let late = if in_flight {
            signed_in(&standard, "a@example.com");
            t.server.existing_checks.remove("claude");
            t.server.check_existing_accounts();
            let Wait::Event(event @ Event::Job(..)) = t.rx.wait(crate::testing::PATIENCE) else {
                panic!("discovery did not finish");
            };
            assert!(Accounts::load().unwrap().reviews().is_empty(), "the worker must not save before validation");
            Some(event)
        } else {
            None
        };
        let (client, _peer) = super::super::tests::client(&mut t.server);
        let login = t.server.open_login(client, &work).unwrap();
        assert!(!t.server.account_identities.contains_key(&work.id));
        assert!(!t.server.existing_checks.contains_key("claude"), "discovery waits while login changes credentials");
        signed_in(home, "b@example.com");
        t.server.check_login(login).unwrap();
        t.until("login is verified", |t| !t.server.panes.contains_key(&login));
        let current = t.server.accounts.by_id(&work.id).unwrap();
        assert_ne!(current.login_revision, work.login_revision);
        assert_eq!(current, Account { login_revision: current.login_revision.clone(), ..work.clone() });
        t.until("post-login discovery finishes", |t| t.server.jobs.len() == usize::from(in_flight));
        signed_in(&standard, "a@example.com");
        t.server.refresh_existing_accounts();
        if let Some(late) = late {
            t.server.handle(late);
        }
        t.until("fresh discovery finishes", |t| t.server.jobs.is_empty());
        let saved = Accounts::load().unwrap();
        assert_eq!(saved.reviews().len(), 1, "A must remain separate from B");
        assert!(saved.entries().iter().all(|a| a.linked_to.as_deref() != Some(&work.id)));
        assert_ne!(saved.by_home("claude", &standard).unwrap().id, work.id);
        assert_eq!(t.server.account_identities[&work.id].1.identity.as_ref().unwrap().email, "b@example.com");
        assert!(t.server.account_notices.is_empty(), "stale discovery must not announce a join");
    }
}

#[test]
fn another_servers_login_invalidates_pending_cached_and_in_flight_discovery() {
    use super::super::{Event, queue::Wait};

    if !crate::testing::in_own_process(
        "server::agents::tests::another_servers_login_invalidates_pending_cached_and_in_flight_discovery",
    ) {
        return;
    }
    for phase in ["pending", "cached", "in-flight"] {
        let (mut t, work, standard) = identity_discovery(&format!("other-login-{phase}"));
        let home = std::path::Path::new(&work.home);
        signed_in(home, "a@example.com");
        signed_in(&standard, if phase == "cached" { "c@example.com" } else { "a@example.com" });
        let mut other = SwitchTest::new(&format!("login-owner-{phase}"));
        other.server.switches.clear();
        other.server.panes.clear();
        other.server.accounts = Accounts::load().unwrap();
        // Suppress discovery until the other server opens its real login pane.
        for agent in accounts::agents() {
            other.server.existing_checks.insert(agent.name.into(), ExistingCheck::Checking);
        }
        let late = if phase == "in-flight" {
            t.server.check_existing_accounts();
            let Wait::Event(event @ Event::Job(..)) = t.rx.wait(crate::testing::PATIENCE) else {
                panic!("discovery did not finish");
            };
            Some(event)
        } else {
            if phase == "cached" {
                t.server.check_existing_accounts();
                t.until("the named identity is cached", |t| t.server.jobs.is_empty());
                assert_eq!(t.server.account_identities[&work.id].1.identity.as_ref().unwrap().email, "a@example.com");
            }
            None
        };
        let (client, _peer) = super::super::tests::client(&mut other.server);
        let login = other.server.open_login(client, &work).unwrap();
        if phase == "pending" {
            t.server.reload_accounts();
            t.until("discovery during login finishes", |t| t.server.jobs.is_empty());
            assert_ne!(Accounts::load().unwrap().by_home("claude", &standard).unwrap().id, work.id);
        }
        signed_in(home, "b@example.com");
        other.server.check_login(login).unwrap();
        other.until("the other server verifies login", |t| !t.server.panes.contains_key(&login));
        other.until("the other server finishes discovery", |t| t.server.jobs.is_empty());
        signed_in(&standard, "a@example.com");
        assert!(
            t.server.account_identity_generations.is_empty(),
            "another server cannot invalidate the local generation"
        );
        if let Some(late) = late {
            t.server.handle(late);
        } else {
            // Keep the old cached account snapshot to exercise revalidation at commit too.
            t.server.existing_checks.remove("claude");
            t.server.check_existing_accounts();
        }
        t.until("discovery finishes", |t| t.server.jobs.is_empty());
        let saved = Accounts::load().unwrap();
        assert_eq!(saved.reviews().len(), 1, "{phase}: A stays separate from B");
        assert_ne!(saved.by_home("claude", &standard).unwrap().id, work.id);
        assert!(saved.entries().iter().all(|a| a.linked_to.as_deref() != Some(&work.id)));
        assert!(t.server.account_notices.is_empty());
    }
}

#[test]
fn a_failed_automatic_join_on_a_saved_review_can_retry_and_recover() {
    use super::super::{Event, queue::Wait};

    if !crate::testing::in_own_process(
        "server::agents::tests::a_failed_automatic_join_on_a_saved_review_can_retry_and_recover",
    ) {
        return;
    }
    let (mut t, work, standard) = identity_discovery("review-save-retry");
    signed_in(std::path::Path::new(&work.home), "a@example.com");
    signed_in(&standard, "a@example.com");
    let existing = t.server.accounts.get("claude", "default").unwrap();
    t.server.accounts.adopt_checked(&existing, None).unwrap();
    t.server.accounts.save().unwrap();
    t.server.check_existing_accounts();
    let Wait::Event(event @ Event::Job(..)) = t.rx.wait(crate::testing::PATIENCE) else {
        panic!("discovery did not finish");
    };
    let path = Accounts::path();
    let backup = path.with_extension("kept");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    t.server.handle(event);
    t.until("the save fails", |t| t.server.jobs.is_empty());
    assert!(matches!(t.server.existing_checks.get("claude"), Some(ExistingCheck::SaveFailed)));
    assert_eq!(t.server.accounts.reviews().len(), 1);
    assert!(t.server.account_notices.is_empty());
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    t.server.refresh_existing_accounts();
    let mut picker = t.server.accounts_picker(None);
    picker.select(|i| i.detail == "Could not save · Check again");
    let item = picker.current().unwrap();
    assert!(matches!(&item.action, PickAction::CheckExisting { agent } if agent == "claude"));
    assert_eq!(picker.action_keys(), &[("enter", "Check again")]);
    t.server.pick(99, &picker, item.clone());
    t.until("retry joins the saved review", |t| t.server.jobs.is_empty());
    assert!(t.server.accounts.reviews().is_empty());
    let saved = Accounts::load().unwrap();
    assert_eq!(saved.get("claude", "default").unwrap().id, work.id);
    assert_eq!(saved.all().len(), 1);
    assert_eq!(t.server.account_notices, ["~/.claude is Claude · work"]);
}

#[test]
fn a_verified_account_opens_review_without_rechecking_or_waiting_for_another_agent() {
    let mut t = SwitchTest::new("review-checked");
    let original = t.server.accounts.get("claude", "default").unwrap();
    t.server.accounts.adopt_checked(&original, None).unwrap();
    t.server.existing_checks.insert("codex".into(), ExistingCheck::Checking);
    t.server.existing_checks.insert("claude".into(), ExistingCheck::Checked(Status::SignedIn));
    for _ in 0..3 {
        t.server.refresh_existing_accounts();
        let picker = t.server.accounts_picker(None);
        let item = picker.items.iter().find(|i| matches!(i.action, PickAction::ReviewAccount(_))).unwrap();
        assert_eq!(item.label, "? Which account is ~/.claude");
        assert!(item.detail.is_empty());
        assert!(item.dim);
        let Mode::Picker(review) = t.server.pick(99, &picker, item.clone()) else { panic!("review") };
        assert!(review.kind == PickerKind::AccountReview);
        assert!(t.server.jobs.is_empty(), "a verified home needs no new status command to open Review");
    }
}

#[test]
fn review_questions_and_confirmations_fit_both_terminal_sizes() {
    use ratatui::{buffer::Buffer, layout::Rect};
    let mut t = SwitchTest::new("review-ui");
    t.server.accounts.rename("claude", "from", "patron.bzh").unwrap();
    t.server.accounts.rename("claude", "to", "gmail").unwrap();
    let original = t.server.accounts.get("claude", "default").unwrap();
    t.server.accounts.adopt_checked(&original, None).unwrap();
    let existing = t.server.accounts.reviews().pop().unwrap();
    for (cols, rows) in [(124, 36), (80, 24)] {
        let screen = Rect::new(0, 0, cols, rows);
        let picker = t.server.review_account_picker(&existing);
        assert_eq!(picker.action_keys(), &[("enter", "Choose")]);
        let mut buf = Buffer::empty(screen);
        super::super::overlays::draw_picker(&picker, screen, &mut buf);
        let text: String = crate::testing::buffer_text(&buf);
        for label in
            ["Existing Claude account", "Which account is this?", "patron.bzh", "gmail", "A different account", "Back"]
        {
            assert!(text.contains(label), "{cols}x{rows}: {label}");
        }
        let mode = t.server.pick(99, &picker, picker.items[0].clone());
        let Mode::Confirm(question) = mode else { panic!("choosing only opens confirmation") };
        assert!(!question.yes);
        assert_eq!(question.action.verb(), "Use account");
        super::super::overlays::draw_confirm(&question, screen, &mut buf);
        let text: String = crate::testing::buffer_text(&buf);
        for label in [
            "Use patron.bzh?",
            "Your running agents will keep working.",
            "This account will appear once.",
            "Cancel",
            "Use account",
        ] {
            assert!(text.contains(label), "{cols}x{rows}: {label}");
        }
        assert_eq!(t.server.accounts.reviews().len(), 1, "choosing does not commit the review");
    }
}

#[test]
fn a_linked_live_pane_keeps_its_home_and_selecting_its_identity_does_not_switch() {
    let mut t = SwitchTest::new("review-pane");
    t.server.switches.clear();
    let old = t.server.accounts.get("claude", "default").unwrap();
    t.server.accounts.adopt_checked(&old, None).unwrap();
    let pending = t.server.accounts.reviews().pop().unwrap();
    let target = t.server.accounts.get("claude", "to").unwrap();
    let spec = &mut t.server.panes.get_mut(&1).unwrap().spec;
    spec.account = Some("claude:default".into());
    spec.account_id = Some(old.id.clone());
    spec.env = accounts::launch(&old, &[], None, None).unwrap().env;
    let command = spec.command.clone();
    let before = t.server.account_display_at(1_000);
    t.server.accounts.review(&pending, Some(&target)).unwrap();
    t.server.follow_renamed_accounts();
    assert_eq!(t.server.pane_account(1).unwrap().home, old.home);
    assert_eq!(t.server.pane_account(1).unwrap().id, target.id);
    let after = t.server.account_display_at(1_000);
    assert!(!std::rc::Rc::ptr_eq(&before, &after));
    assert_eq!(after.labels[&1], accounts::label(&target));
    assert_eq!(t.server.panes[&1].spec.command, command);
    assert!(t.server.panes[&1].exit_code.is_none());
    t.server.switch_account(1, &target, None, false).unwrap();
    assert!(t.server.switches.is_empty());
    assert_eq!(t.server.panes[&1].agent.state, AgentState::Working);
}

#[test]
fn unrestored_accounts_follow_renames_but_never_replacements() {
    let mut t = SwitchTest::new("unrestored");
    let old = t.server.accounts.get("claude", "from").unwrap();
    let pane = serde_json::json!({ "name": "agent", "agent": "claude", "agent_session": null,
            "spec": { "account": "claude:from", "env": [["CLAUDE_CONFIG_DIR", old.home]] } });
    let ws = serde_json::json!({ "name": "failed", "definition": null, "cwd": null, "active": 0,
            "tabs": [{ "name": "", "root": { "Split": { "axis": "Row", "children": [[{ "Pane": pane }, 1.0]] } },
                "floating": [[pane, { "x": 0.1, "y": 0.1, "w": 0.5, "h": 0.5 }]],
                "focus": null, "show_floating": true, "zoomed": false, "sync": false }] });
    let ws: crate::persist::WorkspaceSnap = serde_json::from_value(ws).unwrap();
    t.server.env.shell = t.dir.join("missing-shell").display().to_string();
    crate::server::persistence::apply(
        &mut t.server,
        crate::persist::SessionSnapshot {
            version: 2,
            workspaces: vec![ws.clone()],
            workspace_order: Vec::new(),
            deleted_workspaces: Vec::new(),
            removed_sources: Vec::new(),
            tunnels: Vec::new(),
        },
    )
    .unwrap();
    assert_eq!(t.server.unrestored.len(), 1, "the missing shell prevents restoration");
    assert_eq!(
        t.server.unrestored[0].tabs[0].floating[0].0.spec.account_id.as_deref(),
        Some(old.id.as_str()),
        "even panes not reached by restore must already be bound"
    );
    // A legacy workspace retained before a reload is bound too.
    t.server.unrestored.push(ws);
    t.server.follow_renamed_accounts();
    t.server.accounts.remove("claude", "from");
    t.server.accounts.add(Account { name: "renamed".into(), ..old.clone() }).unwrap();
    t.server.follow_renamed_accounts();
    assert!(t.server.persist_dirty);
    let check = |server: &Server| {
        for ws in &server.unrestored {
            let crate::persist::NodeSnap::Split { children, .. } = ws.tabs[0].root.as_ref().unwrap() else {
                panic!("split")
            };
            let crate::persist::NodeSnap::Pane(tiled) = &children[0].0 else { panic!("pane") };
            for pane in [tiled.as_ref(), &ws.tabs[0].floating[0].0] {
                assert_eq!(pane.spec.account_id.as_deref(), Some(old.id.as_str()));
                assert_eq!(pane.spec.account.as_deref(), Some("claude:renamed"));
                assert!(pane.spec.env.contains(&("KRAAL_ACCOUNT_ID".into(), old.id.clone())));
            }
        }
    };
    check(&t.server);
    t.server.accounts.remove("claude", "renamed");
    t.server.accounts.add(Account { id: crate::identity::new_uuid(), name: "renamed".into(), ..old.clone() }).unwrap();
    t.server.follow_renamed_accounts();
    check(&t.server);
    assert!(t.server.accounts.by_id(&old.id).is_none());
}

#[test]
fn cached_labels_follow_observed_accounts_and_legacy_launch_environments() {
    let mut t = SwitchTest::new("cached-observed");
    t.server.switches.clear();
    let from = t.server.accounts.get("claude", "from").unwrap();
    let to = t.server.accounts.get("claude", "to").unwrap();
    let spec = &mut t.server.panes.get_mut(&1).unwrap().spec;
    spec.account = None;
    spec.account_id = None;
    assert_eq!(t.server.pane_account_label(1), None);
    t.server.observed_agents.insert(
        1,
        ObservedAgent {
            pid: 1,
            started: 1,
            command: vec!["claude".into()],
            account: from.clone(),
            session: "test".into(),
            cwd: t.dir.clone(),
            path: String::new(),
        },
    );
    assert_eq!(t.server.pane_account_label(1), Some(accounts::label(&from)));
    t.server.observed_agents.get_mut(&1).unwrap().account = to.clone();
    assert_eq!(t.server.pane_account_label(1), Some(accounts::label(&to)));
    t.server.observed_agents.clear();
    assert_eq!(t.server.pane_account_label(1), None);
    let spec = &mut t.server.panes.get_mut(&1).unwrap().spec;
    spec.account = Some("claude:old-name".into());
    spec.env = vec![("CLAUDE_CONFIG_DIR".into(), from.home.clone())];
    assert_eq!(t.server.pane_account_label(1), Some(accounts::label(&from)));
    t.server.panes.get_mut(&1).unwrap().spec.env = vec![("CLAUDE_CONFIG_DIR".into(), to.home.clone())];
    assert_eq!(t.server.pane_account_label(1), Some(accounts::label(&to)));
}

struct SwitchTest {
    server: Server,
    dir: PathBuf,
    rx: super::super::queue::EventReceiver,
}

impl SwitchTest {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("kraal-switch-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("from/projects/work")).unwrap();
        std::fs::create_dir_all(dir.join("to/projects/work")).unwrap();
        std::fs::write(dir.join("from/projects/work/session.jsonl"), "{\"message\":1}\n").unwrap();
        let dir = dir.canonicalize().unwrap();
        let account = |name: &str| Account {
            id: crate::identity::new_uuid(),
            agent: "claude".into(),
            name: name.into(),
            home: dir.join(name).display().to_string(),
            threshold: None,
            note: None,
            sign_in_required: false,
            login_revision: String::new(),
            review: false,
            public_identity: None,
            linked_to: None,
            references: Vec::new(),
        };
        let (from, to) = (account("from"), account("to"));
        let (tx, rx) = super::super::queue::channel(64);
        let mut server = super::super::tests::server(tx);
        server.config.notifications.mode = crate::config::NotifyMode::Off;
        server.accounts.add(from.clone()).unwrap();
        server.accounts.add(to.clone()).unwrap();
        let spec = PaneSpec {
            command: vec!["claude".into()],
            account: Some("claude:from".into()),
            account_id: Some(from.id.clone()),
            session: Some("session".into()),
            text: Some(crate::pane::ShownText { title: String::new(), text: "still running".into() }),
            ..Default::default()
        };
        let mut pane = crate::pane::Pane::spawn(1, None, spec, 80, 24, &server.env, &server.tx).unwrap();
        pane.agent.hooked = true;
        pane.agent.state = AgentState::Working;
        server.panes.insert(1, pane);
        server.next_id = 10;
        server.switches.insert(
            1,
            AccountSwitch {
                token: 2,
                from,
                to,
                session: None,
                process: None,
                client: Some(99),
                ask: false,
                original: server.panes[&1].spec.clone(),
                undo: false,
                observed: None,
                started: None,
                stopping: None,
                stage: SwitchStage::Checking,
            },
        );
        Self { server, dir, rx }
    }

    fn pump(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.server.handle(event);
        }
        while let Some(data) = self.server.panes.get(&1).and_then(|p| p.take_output()) {
            self.server.pane_output(1, &data);
        }
        self.server.finish_exits();
    }

    fn until(&mut self, what: &str, mut check: impl FnMut(&Self) -> bool) {
        let start = Instant::now();
        loop {
            self.pump();
            if check(self) {
                return;
            }
            assert!(start.elapsed() < crate::testing::PATIENCE, "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn spawn(&mut self, spec: PaneSpec) {
        let pane = crate::pane::Pane::spawn(1, None, spec, 80, 24, &self.server.env, &self.server.tx).unwrap();
        self.server.panes.insert(1, pane);
    }

    fn checked(&mut self, status: Status) {
        self.server.switch_checked(1, 2, status, false);
    }

    fn unchanged(&self) {
        let p = &self.server.panes[&1];
        assert_eq!(p.spec.account.as_deref(), Some("claude:from"));
        assert!(p.exit_code.is_none());
        assert_eq!(p.agent.state, AgentState::Working);
    }
}

impl Drop for SwitchTest {
    fn drop(&mut self) {
        for switch in self.server.switches.values() {
            if let Some(stop) = &switch.stopping {
                stop.signal(libc::SIGKILL);
            }
        }
        for pane in self.server.panes.values() {
            if let Some(stop) = ProgramStop::new(pane) {
                stop.signal(libc::SIGKILL);
            }
        }
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[test]
fn only_reliable_turn_boundaries_allow_an_unconfirmed_switch() {
    for state in [AgentState::Unknown, AgentState::Working, AgentState::Blocked, AgentState::Idle, AgentState::Done] {
        assert_eq!(switch_timing(false, state, false), SwitchStage::Confirm);
        assert_eq!(switch_timing(false, state, true), SwitchStage::Stopping);
    }
    for state in [AgentState::Working, AgentState::Blocked] {
        assert_eq!(switch_timing(true, state, false), SwitchStage::Waiting);
    }
    for state in [AgentState::Idle, AgentState::Done] {
        assert_eq!(switch_timing(true, state, false), SwitchStage::Stopping);
    }
    assert_eq!(switch_timing(true, AgentState::Unknown, false), SwitchStage::Confirm);
}

#[test]
fn picker_effects_distinguish_sign_in_continuation_and_uncertainty() {
    for agent in ["claude", "codex"] {
        assert_eq!(switch_effect(agent, Status::NotSignedIn, true), "Sign-in required");
        for status in [Status::NotInstalled, Status::CouldNotCheck] {
            assert_eq!(switch_effect(agent, status, true), "Could not verify");
        }
    }
    assert_eq!(switch_effect("claude", Status::SignedIn, true), "Continue conversation");
    assert_eq!(switch_effect("claude", Status::SignedIn, false), "Could not verify");
    assert_eq!(switch_effect("codex", Status::SignedIn, false), "New conversation");
}

#[test]
fn failed_verification_leaves_the_current_agent_untouched() {
    for (i, status) in [Status::NotSignedIn, Status::NotInstalled, Status::CouldNotCheck].into_iter().enumerate() {
        let mut t = SwitchTest::new(&format!("status-{i}"));
        t.checked(status);
        t.unchanged();
        assert!(t.server.switches.is_empty());
    }
}

#[test]
fn missing_unreadable_and_divergent_transcripts_refuse_before_stopping() {
    use std::os::unix::fs::PermissionsExt;
    for reason in ["missing", "empty", "unreadable", "diverged", "unknown-id"] {
        let mut t = SwitchTest::new(reason);
        let src = t.dir.join("from/projects/work/session.jsonl");
        match reason {
            "missing" => std::fs::remove_file(&src).unwrap(),
            "empty" => std::fs::write(&src, "").unwrap(),
            "unreadable" => {
                std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
                if std::fs::read(&src).is_ok() {
                    continue;
                }
            }
            "diverged" => std::fs::write(t.dir.join("to/projects/work/session.jsonl"), "different\n").unwrap(),
            _ => t.server.panes.get_mut(&1).unwrap().agent.session_id = Some("../invalid".into()),
        }
        t.checked(Status::SignedIn);
        t.unchanged();
        assert!(t.server.switches.is_empty(), "{reason}");
    }
}

#[test]
fn a_queued_switch_is_shared_cancellable_and_ignores_late_checks() {
    let mut t = SwitchTest::new("cancel");
    let to = t.server.switches[&1].to.clone();
    assert!(t.server.switch_account(1, &to, Some(99), false).unwrap_err().to_string().contains("already pending"));
    t.checked(Status::SignedIn);
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Waiting);
    t.unchanged();
    let Mode::Picker(p) = t.server.pending_switch_mode(1) else { panic!("cancellable") };
    assert!(p.items[0].detail.contains("Switch after this turn"));
    assert!(matches!(p.items[0].action, PickAction::CancelSwitch { pane: 1, token: 2 }));
    t.server.cancel_switch(1, 2);
    t.checked(Status::SignedIn);
    assert!(t.server.switches.is_empty());
    t.unchanged();
}

#[test]
fn queued_changes_recheck_the_current_conversation_and_account() {
    for changed in ["transcript", "session", "target", "replaced"] {
        let mut t = SwitchTest::new(&format!("changed-{changed}"));
        t.checked(Status::SignedIn);
        match changed {
            "transcript" => std::fs::remove_file(t.dir.join("from/projects/work/session.jsonl")).unwrap(),
            "session" => t.server.panes.get_mut(&1).unwrap().agent.session_id = Some("new-session".into()),
            _ => {
                let old = t.server.accounts.get("claude", "to").unwrap();
                t.server.accounts.remove("claude", "to");
                if changed == "replaced" {
                    t.server.accounts.add(Account { id: crate::identity::new_uuid(), ..old }).unwrap();
                }
            }
        }
        t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Checking;
        t.checked(Status::SignedIn);
        t.unchanged();
        assert!(t.server.switches.is_empty(), "{changed}");
    }
}

#[test]
fn an_automatic_change_rechecks_headroom_before_stopping() {
    for reason in ["unknown", "full", "sign-in", "command"] {
        let mut t = SwitchTest::new(&format!("auto-headroom-{reason}"));
        t.server.switches.get_mut(&1).unwrap().client = None;
        if reason != "unknown" {
            t.server.usage.0.insert(
                t.server.accounts.get("claude", "to").unwrap().key(),
                accounts::Usage {
                    five_hour: Some(accounts::Window {
                        used_percent: if reason == "full" { 100.0 } else { 0.0 },
                        resets_at: Some(accounts::now() + 3600),
                    }),
                    ..Default::default()
                },
            );
        }
        if reason == "sign-in" {
            t.server.accounts.set_sign_in_required(&t.server.switches[&1].to, true);
        }
        if reason == "command" {
            t.server.panes.get_mut(&1).unwrap().spec.command = vec!["claude".into(), "a prompt".into()];
        }
        t.checked(Status::SignedIn);
        assert!(t.server.switches.is_empty(), "{reason}");
        t.unchanged();
    }
}

#[test]
fn ask_and_unreliable_turns_require_one_confirmation_before_stopping() {
    for ask in [false, true] {
        let mut t = SwitchTest::new(if ask { "ask" } else { "unreliable" });
        t.server.switches.get_mut(&1).unwrap().ask = ask;
        let p = t.server.panes.get_mut(&1).unwrap();
        p.agent.hooked = ask;
        p.agent.state = AgentState::Idle;
        t.checked(Status::SignedIn);
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Confirm);
        let Mode::Confirm(q) = t.server.pending_switch_mode(1) else { panic!("confirmation") };
        assert!(!q.yes);
        t.server.prepare_switch(1, true).unwrap();
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Stopping);
        t.server.cancel_switch(1, 2);
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Stopping);
    }
}

#[test]
fn a_late_check_does_not_touch_a_closed_pane_or_a_replacement_change() {
    let mut t = SwitchTest::new("late-check");
    t.server.switches.get_mut(&1).unwrap().token = 3;
    t.checked(Status::SignedIn);
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Checking);
    t.server.panes.remove(&1);
    t.server.switch_checked(1, 3, Status::SignedIn, false);
    assert!(t.server.switches.is_empty());
}

#[test]
fn another_name_for_the_current_home_is_also_a_no_op() {
    let mut t = SwitchTest::new("same-home");
    let mut alias = t.server.switches.remove(&1).unwrap().from;
    alias.name = "alias".into();
    t.server.switch_account(1, &alias, Some(99), false).unwrap();
    assert!(t.server.switches.is_empty());
    assert!(t.server.jobs.is_empty());
    t.unchanged();
}

#[test]
fn stop_confirmation_defaults_to_cancel_and_codex_names_the_new_conversation() {
    let mut t = SwitchTest::new("confirm");
    let switch = t.server.switches.get_mut(&1).unwrap();
    switch.stage = SwitchStage::Confirm;
    switch.to.agent = "codex".into();
    let Mode::Confirm(q) = t.server.pending_switch_mode(1) else { panic!("confirmation") };
    assert!(!q.yes);
    assert!(q.message.starts_with("Stop and switch?"));
    assert!(q.message.contains("New conversation"));
    t.server.confirm_switch(1, 2, 99, false);
    assert!(t.server.switches.is_empty());
    t.unchanged();
}

#[test]
fn an_automatic_failure_is_remembered_until_usage_falls_below_the_threshold() {
    let mut t = SwitchTest::new("auto-failure");
    t.server.accounts.limits.mode = LimitMode::Auto;
    let usage = |percent| accounts::Usage {
        five_hour: Some(accounts::Window { used_percent: percent, resets_at: Some(accounts::now() + 3600) }),
        ..Default::default()
    };
    t.server.usage.0.insert(t.server.accounts.get("claude", "from").unwrap().key(), usage(95.0));
    t.server.usage.0.insert(t.server.accounts.get("claude", "to").unwrap().key(), usage(0.0));
    t.server.limit_alerts.insert(1, LimitAlert::Attempted);
    t.checked(Status::NotSignedIn);
    t.server.panes.get_mut(&1).unwrap().agent.state = AgentState::Done;
    for _ in 0..10 {
        t.server.check_limits();
    }
    assert!(t.server.switches.is_empty());
    assert!(t.server.jobs.is_empty());
    assert_eq!(t.server.limit_alerts.get(&1), Some(&LimitAlert::Attempted));
    t.server.usage.0.insert(t.server.accounts.get("claude", "from").unwrap().key(), usage(10.0));
    t.server.check_limits();
    assert!(!t.server.limit_alerts.contains_key(&1));
}
#[test]
fn observed_commands_require_exact_interactive_arguments() {
    let words = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    for command in [
        "claude --model opus",
        "/usr/bin/node /opt/claude --resume session --model opus",
        "sh /bin/codex resume original -m model",
    ] {
        let kind = if command.contains("codex") { "codex" } else { "claude" };
        assert!(observed_command(&words(command), kind).is_some(), "{command}");
    }
    for command in [
        "sh -c claude",
        "wrapper claude",
        "node --eval claude",
        "claude --new-flag value",
        "claude -p prompt",
        "claude --settings other",
        "claude --resume session --settings other",
        "claude --continue",
        "claude --fork-session",
    ] {
        assert!(observed_command(&words(command), "claude").is_none(), "{command}");
    }
    for command in ["codex exec prompt", "codex -c auth=other", "codex --profile other", "codex resume --last"] {
        assert!(observed_command(&words(command), "codex").is_none(), "{command}");
    }
}

#[test]
fn undo_is_saved_only_with_a_started_change_and_survives_resuming_the_pane() {
    let mut t = SwitchTest::new("undo-saved");
    t.server.limit_alerts.insert(1, LimitAlert::Notified);
    t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Stopping;
    t.server.switches.get_mut(&1).unwrap().session = Some("session".into());
    t.server.finish_switch(1);
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Starting);
    assert!(t.server.messages.is_empty(), "preparing a command is not success");
    t.server.switch_started(1);
    assert!(t.server.switches.is_empty());
    assert!(!t.server.limit_alerts.contains_key(&1), "the new account has its own threshold crossing");
    let spec = &t.server.panes[&1].spec;
    let saved = spec.account_undo.as_ref().unwrap();
    assert_eq!(saved.session.as_deref(), Some("session"));
    assert_eq!(saved.account.as_deref(), Some("claude:from"));
    assert!(saved.account_undo.is_none());
    let json = serde_json::to_string(spec).unwrap();
    let restored: PaneSpec = serde_json::from_str(&json).unwrap();
    let (resumed, _) = t.server.resume_spec(&restored, Some("session"));
    assert_eq!(resumed.account_undo.unwrap().account_id, saved.account_id);
    assert!(
        serde_json::from_str::<PaneSpec>(r#"{"command":["claude"],"account":"claude:from"}"#)
            .unwrap()
            .account_undo
            .is_none()
    );
}

#[test]
fn claude_undo_carries_the_latest_transcript_and_refuses_divergence() {
    for diverged in [false, true] {
        let mut t = SwitchTest::new(if diverged { "undo-diverged" } else { "undo-latest" });
        t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Stopping;
        t.server.switches.get_mut(&1).unwrap().session = Some("session".into());
        t.server.finish_switch(1);
        t.server.switch_started(1);
        let original = t.server.panes[&1].spec.clone();
        let from = t.server.accounts.get("claude", "to").unwrap();
        let to = t.server.accounts.get("claude", "from").unwrap();
        std::fs::write(t.dir.join("to/projects/work/session.jsonl"), "{\"message\":1}\n{\"message\":2}\n").unwrap();
        if diverged {
            std::fs::write(t.dir.join("from/projects/work/session.jsonl"), "different\n").unwrap();
        }
        t.server.switches.insert(
            1,
            AccountSwitch {
                token: 3,
                from,
                to,
                original,
                session: None,
                process: None,
                client: Some(99),
                ask: false,
                undo: true,
                observed: None,
                started: None,
                stopping: None,
                stage: SwitchStage::Checking,
            },
        );
        let result = t.server.prepare_switch(1, true);
        if diverged {
            assert!(result.is_err());
            assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:to"));
        } else {
            result.unwrap();
            t.server.finish_switch(1);
            t.server.switch_started(1);
            assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
            assert!(t.server.panes[&1].spec.account_undo.is_none());
            assert_eq!(
                std::fs::read(t.dir.join("from/projects/work/session.jsonl")).unwrap(),
                std::fs::read(t.dir.join("to/projects/work/session.jsonl")).unwrap()
            );
        }
    }
}

#[test]
fn failed_start_restores_the_source_and_a_failed_recovery_keeps_one_retry() {
    let mut t = SwitchTest::new("restore-source");
    t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Starting;
    let target = t.server.switches[&1].to.clone();
    t.server.panes.get_mut(&1).unwrap().spec = t.server.agent_spec(&target, None, Some("session"), &[]).unwrap();
    t.server.switch_start_failed(1, "target exited");
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Restoring);
    assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
    assert!(t.server.panes[&1].spec.command.windows(2).any(|w| w == ["--resume", "session"]));
    assert!(t.server.panes[&1].spec.account_undo.is_none());
    t.server.switch_start_failed(1, "source exited too");
    assert!(t.server.switches.is_empty());
    assert!(t.server.panes[&1].exit_code.is_some());
    assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
    t.server.check_switches();
    assert!(t.server.switches.is_empty(), "no retry loop");
}

#[test]
fn an_unassociated_process_offers_a_new_pane_without_changing_the_old_one() {
    let mut t = SwitchTest::new("opaque");
    t.server.switches.clear();
    t.server.panes.get_mut(&1).unwrap().spec.account = None;
    t.server.panes.get_mut(&1).unwrap().spec.account_id = None;
    let Mode::Picker(p) = t.server.change_account_mode(99, 1) else { panic!("a way out") };
    assert_eq!(p.title, "Can't switch this process.");
    assert!(matches!(p.items[0].action, PickAction::OpenAnotherAccount(1)));
    assert_eq!(p.items[0].label, "Open with another account");
    assert_eq!(t.server.panes[&1].agent.state, AgentState::Working);
    assert!(t.server.switches.is_empty());
}

#[test]
fn notify_threshold_has_a_change_action_without_stopping_or_requiring_an_alternative() {
    let mut t = SwitchTest::new("notify-action");
    t.server.switches.clear();
    let (client, _peer) = super::super::tests::client(&mut t.server);
    let a = t.server.accounts.get("claude", "from").unwrap();
    t.server.usage.0.insert(
        a.key(),
        accounts::Usage {
            five_hour: Some(accounts::Window { used_percent: 94.6, resets_at: Some(accounts::now() + 3600) }),
            ..Default::default()
        },
    );
    t.server.check_limits();
    let c = &t.server.clients[&client];
    assert_eq!(c.toast.as_ref().unwrap().0, "from is at 95% used · Change account");
    assert_eq!(c.toast_action, Some((1, super::super::attention::ToastAction::ChangeAccount)));
    t.unchanged();
    assert!(t.server.switches.is_empty());
    t.server.toast(client, NoticeKind::Notice, "Dismissed");
    t.server.check_limits();
    assert_eq!(t.server.clients[&client].toast.as_ref().unwrap().0, "Dismissed", "one notification per crossing");
}
#[test]
fn codex_undo_resumes_the_original_id_and_keeps_both_histories() {
    for completed in [true, false] {
        let mut t = SwitchTest::new(if completed { "codex-return" } else { "codex-unused-return" });
        let s = t.server.switches.get_mut(&1).unwrap();
        s.from.agent = "codex".into();
        s.to.agent = "codex".into();
        let (from, to) = (s.from.clone(), s.to.clone());
        t.server.accounts = Accounts::empty();
        t.server.accounts.add(from.clone()).unwrap();
        t.server.accounts.add(to.clone()).unwrap();
        for (home, session) in [("from", "original"), ("to", "new")] {
            let dir = t.dir.join(home).join("sessions/2026/10/13");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("rollout-today-{session}.jsonl")),
                format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\"}}}}\n"),
            )
            .unwrap();
        }
        let spec = t.server.agent_spec(&from, None, Some("original"), &[]).unwrap();
        t.server.panes.get_mut(&1).unwrap().spec = spec.clone();
        let s = t.server.switches.get_mut(&1).unwrap();
        s.original = spec;
        s.session = Some("original".into());
        s.stage = SwitchStage::Stopping;
        t.server.finish_switch(1);
        assert_eq!(t.server.panes[&1].agent.session_id, None, "new Codex never inherits the original id");
        t.server.switch_started(1);
        if completed {
            t.server.panes.get_mut(&1).unwrap().agent.session_id = Some("new".into());
        }
        let original = t.server.panes[&1].spec.clone();
        t.server.switches.insert(
            1,
            AccountSwitch {
                token: 3,
                from: to.clone(),
                to: from.clone(),
                original,
                session: None,
                process: None,
                client: Some(99),
                ask: false,
                undo: true,
                observed: None,
                started: None,
                stopping: None,
                stage: SwitchStage::Checking,
            },
        );
        t.server.prepare_switch(1, true).unwrap();
        t.server.finish_switch(1);
        assert!(t.server.panes[&1].spec.command.windows(2).any(|w| w == ["resume", "original"]));
        t.server.switch_started(1);
        accounts::check_resume(&from, "original").unwrap();
        accounts::check_resume(&to, "new").unwrap();
        assert!(t.server.panes[&1].spec.account_undo.is_none());
    }
}

#[test]
fn a_failed_undo_sign_in_keeps_the_return_action_and_running_account() {
    let mut t = SwitchTest::new("undo-sign-in");
    t.server.switches.get_mut(&1).unwrap().undo = true;
    t.server.panes.get_mut(&1).unwrap().spec.account_undo = Some(Box::new(PaneSpec {
        account: Some("claude:to".into()),
        session: Some("original".into()),
        ..Default::default()
    }));
    t.checked(Status::NotSignedIn);
    t.unchanged();
    assert!(t.server.panes[&1].spec.account_undo.is_some());
    assert!(t.server.switches.is_empty());
}

#[test]
fn a_live_detected_process_is_rechecked_before_any_stop() {
    let mut t = SwitchTest::new("observed-process");
    let (client, _peer) = super::super::tests::client(&mut t.server);
    t.server.switches.get_mut(&1).unwrap().client = Some(client);
    let program = t.dir.join("claude");
    crate::testing::write_script(&program, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n");
    let from = t.server.switches[&1].from.clone();
    let spec = PaneSpec {
        command: vec![program.display().to_string(), "--session-id".into(), "session".into()],
        env: vec![("CLAUDE_CONFIG_DIR".into(), from.home.clone())],
        cwd: Some(t.dir.clone()),
        ..Default::default()
    };
    let pane = crate::pane::Pane::spawn(1, None, spec, 80, 24, &t.server.env, &t.server.tx).unwrap();
    t.server.panes.insert(1, pane);
    let started = Instant::now();
    let observed = loop {
        t.server.tick(Instant::now());
        if let Ok(Some(a)) = t.server.observed_agent(1) {
            break a;
        }
        assert!(started.elapsed() < crate::testing::PATIENCE);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(observed.account.id, from.id);
    assert_eq!(observed.session, "session");
    assert!(!t.server.panes[&1].agent.hooked);
    t.server.refresh_observed_accounts();
    assert_eq!(t.server.pane_account(1).unwrap().id, from.id);
    // A failed detection between scans must not forget the kind needed to verify the account.
    t.server.last_scan = Instant::now();
    crate::process::FAIL_ARGV.with(|fail| fail.set(true));
    t.server.tick(t.server.last_scan);
    assert!(!crate::process::FAIL_ARGV.with(|fail| fail.get()));
    assert_eq!(t.server.panes[&1].agent.kind.as_deref(), Some("claude"));
    // The scan also keeps the cached label when both process reads fail.
    crate::process::FAIL_ARGV.with(|fail| fail.set(true));
    crate::process::FAIL_COMMAND_ENV.with(|fail| fail.set(true));
    t.server.last_scan = Instant::now() - super::super::SCAN_EVERY;
    t.server.tick(Instant::now());
    assert!(!crate::process::FAIL_ARGV.with(|fail| fail.get()));
    assert!(!crate::process::FAIL_COMMAND_ENV.with(|fail| fail.get()));
    assert_eq!(t.server.pane_account(1).unwrap().id, from.id, "an unreadable process keeps its account");
    let switch = t.server.switches.remove(&1).unwrap();
    crate::process::FAIL_COMMAND_ENV.with(|fail| fail.set(true));
    assert!(matches!(t.server.change_account_mode(client, 1), Mode::Normal));
    assert_eq!(t.server.pane_account(1).unwrap().id, from.id);
    crate::process::FAIL_COMMAND_ENV.with(|fail| fail.set(true));
    assert_eq!(
        t.server.begin_switch(1, &switch.to, Some(client), false, false).unwrap_err().to_string(),
        PROCESS_UNVERIFIED
    );
    assert!(t.server.switches.is_empty());
    t.server.switches.insert(1, switch);
    let hold = t.server.panes[&1].process_hold();
    let s = t.server.switches.get_mut(&1).unwrap();
    s.process = hold;
    s.observed = Some(observed.clone());
    crate::process::FAIL_COMMAND_ENV.with(|fail| fail.set(true));
    let result = t.server.prepare_switch(1, true);
    assert!(result.unwrap_err().to_string().contains("Could not verify the running agent"));
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Checking);
    assert!(crate::process::still_running(observed.pid, observed.started));
    t.server.switches.get_mut(&1).unwrap().observed =
        Some(ObservedAgent { started: observed.started.wrapping_add(1), ..observed });
    let result = t.server.prepare_switch(1, true);
    assert!(result.unwrap_err().to_string().contains("Can't switch this process."));
    let pid = t.server.panes[&1].pid().unwrap();
    assert!(crate::process::running(pid as libc::pid_t));
    crate::process::FAIL_COMMAND_ENV.with(|fail| fail.set(true));
    t.server.switch_failed(1, PROCESS_UNVERIFIED, false);
    assert!(matches!(
        &t.server.clients[&client].mode,
        Mode::Confirm(q) if matches!(q.action, ConfirmAction::RetryAccountSwitch { .. })
    ));
    t.server.panes.get_mut(&1).unwrap().kill();
    t.until("the agent exits", |t| t.server.panes[&1].exit_code.is_some());
    t.server.refresh_observed_accounts();
    assert!(t.server.pane_account(1).is_none(), "an ended pane must lose the cached account");
}

#[test]
fn a_shell_switch_waits_for_the_agent_and_its_children_and_keeps_the_agents_directory() {
    let mut t = SwitchTest::new("shell-survivor");
    let program = t.dir.join("claude");
    crate::testing::write_script(
        &program,
        r#"#!/bin/sh
test "$*" = 'auth status' && exit 0
root="$(dirname "$CLAUDE_CONFIG_DIR")"
if test "$CLAUDE_CONFIG_DIR" = "$root/from"; then
    trap '' HUP TERM
    sh -c 'trap "" HUP TERM
        echo $$ > "$1/child"
        while test ! -f "$1/release-child"; do sleep 0.02; done
        printf "{\"message\":2}\n" >> "$2"' sh "$root" "$CLAUDE_CONFIG_DIR/projects/work/session.jsonl" &
    echo $$ > "$root/agent"
    while test ! -f "$root/release-agent"; do sleep 0.02; done
    exit 0
fi
pwd > "$root/replacement-cwd"
echo READY
while IFS= read -r line; do :; done
"#,
    );
    let project = t.dir.join("project-B");
    std::fs::create_dir(&project).unwrap();
    let (from, to) = (t.server.switches[&1].from.clone(), t.server.switches[&1].to.clone());
    t.server.switches.clear();
    t.server.env.shell = "bash".into();
    t.spawn(PaneSpec {
        cwd: Some(t.dir.clone()),
        env: vec![
            ("CLAUDE_CONFIG_DIR".into(), from.home.clone()),
            ("PROJECT_B".into(), project.display().to_string()),
            ("FAKE_AGENT".into(), program.display().to_string()),
        ],
        ..Default::default()
    });
    let pane = t.server.panes.get_mut(&1).unwrap();
    pane.agent.kind = Some("claude".into());
    pane.write_input(b"(cd \"$PROJECT_B\"; exec \"$FAKE_AGENT\" --resume session)\n".to_vec());
    let mut observed = None;
    t.until("the shell's foreground agent is observed", |t| {
        observed = t.server.observed_agent(1).ok().flatten();
        crate::testing::read_complete_lines(t.dir.join("child")).is_some() && observed.is_some()
    });
    let observed = observed.unwrap();
    assert_eq!(observed.cwd, project);
    assert_eq!(t.server.panes[&1].current_cwd(), Some(t.dir.clone()));
    t.server.begin_switch(1, &to, Some(99), false, false).unwrap();
    let token = t.server.switches[&1].token;
    t.server.switch_checked(1, token, Status::SignedIn, false);
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Confirm);
    assert_eq!(t.server.switches[&1].original.cwd, Some(project.clone()));
    t.server.prepare_switch(1, true).unwrap();
    // Keep the survivors alive past the shell's output-drain deadline.
    t.server.switches.get_mut(&1).unwrap().stopping.as_mut().unwrap().since = Instant::now() + crate::testing::PATIENCE;
    t.until("the shell exits while its agent survives", |t| t.server.panes[&1].exit_code.is_some());
    assert!(crate::process::still_running(observed.pid, observed.started));
    let child: u32 = crate::testing::read_complete_lines(t.dir.join("child")).unwrap().trim().parse().unwrap();
    let child_started = crate::process::start_time(child).unwrap();
    let copied = t.dir.join("to/projects/work/session.jsonl");
    t.server.check_switches();
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Stopping);
    assert!(!copied.exists(), "no transfer while the foreground agent runs");
    assert!(!t.dir.join("replacement-cwd").exists());

    std::fs::write(t.dir.join("release-agent"), "").unwrap();
    t.until("the foreground agent exits", |_| !crate::process::still_running(observed.pid, observed.started));
    t.server.check_switches();
    assert!(crate::process::still_running(child, child_started));
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Stopping);
    assert!(!copied.exists(), "the agent's orphaned child can still write the transcript");
    assert!(!t.dir.join("replacement-cwd").exists());

    std::fs::write(t.dir.join("release-child"), "").unwrap();
    t.until("the child finishes writing", |_| !crate::process::still_running(child, child_started));
    t.server.check_switches();
    t.until("the replacement runs", |t| crate::testing::read_complete_lines(t.dir.join("replacement-cwd")).is_some());
    assert_eq!(std::fs::read_to_string(copied).unwrap(), "{\"message\":1}\n{\"message\":2}\n");
    assert_eq!(t.server.panes[&1].spec.cwd, Some(project.clone()));
    assert_eq!(
        crate::testing::read_complete_lines(t.dir.join("replacement-cwd")).unwrap().trim(),
        project.to_str().unwrap()
    );
    // Recovery uses the same verified directory, even though the original pane was a shell.
    std::fs::remove_file(t.dir.join("release-agent")).unwrap();
    std::fs::remove_file(t.dir.join("release-child")).unwrap();
    t.server.panes.get_mut(&1).unwrap().kill();
    t.until("the source is restored", |t| t.server.switches[&1].stage == SwitchStage::Restoring);
    assert_eq!(t.server.panes[&1].spec.cwd, Some(project.clone()));
    let pid = t.server.panes[&1].pid().unwrap();
    t.until("recovery runs in the agent's directory", |_| crate::process::cwd(pid) == Some(project.clone()));
}

#[test]
fn silent_replacement_and_recovery_that_ignore_hangup_are_stopped() {
    let mut t = SwitchTest::new("silent-hangup");
    let program = t.dir.join("claude");
    crate::testing::write_script(
        &program,
        "#!/bin/sh\ntrap '' HUP TERM\necho $$ > \"$CLAUDE_CONFIG_DIR/started\"\nwhile :; do sleep 1; done\n",
    );
    let original = PaneSpec {
        command: vec![program.display().to_string(), "--resume".into(), "session".into()],
        env: vec![("CLAUDE_CONFIG_DIR".into(), t.server.switches[&1].from.home.clone())],
        text: None,
        ..t.server.panes[&1].spec.clone()
    };
    t.spawn(original.clone());
    t.until("the source starts", |t| crate::testing::read_complete_lines(t.dir.join("from/started")).is_some());
    t.server.panes[&1].process_hold().unwrap().signal(libc::SIGKILL);
    t.until("the source stops", |t| t.server.panes[&1].exit_code.is_some());
    std::fs::remove_file(t.dir.join("from/started")).unwrap();
    t.server.switches.get_mut(&1).unwrap().original = original;
    t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Stopping;
    t.server.switches.get_mut(&1).unwrap().session = Some("session".into());
    t.server.finish_switch(1);

    for (home, starting, stopping) in [
        ("to", SwitchStage::Starting, SwitchStage::Recovering),
        ("from", SwitchStage::Restoring, SwitchStage::RecoveryStopping),
    ] {
        t.until("the silent agent starts", |t| {
            crate::testing::read_complete_lines(t.dir.join(home).join("started")).is_some()
        });
        assert_eq!(t.server.switches[&1].stage, starting);
        let pid = t.server.panes[&1].pid().unwrap();
        let started = crate::process::start_time(pid).unwrap();
        assert!(!t.server.panes[&1].has_printed());
        t.server.switches.get_mut(&1).unwrap().started = Some(Instant::now() - Duration::from_secs(11));
        t.server.check_switches();
        assert_eq!(t.server.switches[&1].stage, stopping);
        assert!(crate::process::still_running(pid, started), "the agent ignores hangup");
        t.server.switches.get_mut(&1).unwrap().stopping.as_mut().unwrap().since =
            Instant::now() - Duration::from_secs(3);
        t.server.check_switches();
        t.until("the watchdog kills the unresponsive agent", |_| !crate::process::still_running(pid, started));
        t.until("the stopped launch releases its reservation", |t| {
            t.server.switches.get(&1).is_none_or(|s| s.stage != stopping)
        });
    }
    assert!(t.server.switches.is_empty());
    assert!(t.server.panes[&1].exit_code.is_some());
    assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
    t.server.check_switches();
    assert!(t.server.switches.is_empty(), "a failed recovery does not retry forever");
}

#[test]
fn a_deferred_switch_launch_failure_recovers_without_an_exit_event() {
    let mut t = SwitchTest::new("deferred-switch-failure");
    let program = t.dir.join("claude");
    crate::testing::write_script(&program, "#!/bin/sh\necho READY\nread -r line\n");
    let mut original = t.server.panes[&1].spec.clone();
    original.command[0] = program.display().to_string();
    original.text = None;
    t.server.switches.get_mut(&1).unwrap().original = original;
    t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Starting;
    let missing_interpreter = t.dir.join("broken-claude");
    crate::testing::write_script(&missing_interpreter, "#!/nonexistent-interpreter\n");
    let batch = crate::pane::batch();
    t.spawn(PaneSpec { command: vec![missing_interpreter.display().to_string()], ..Default::default() });
    drop(batch);
    let pending = t.server.panes.get_mut(&1).unwrap().take_pending().unwrap();
    let crate::pane::Started::Failed(id, process, why) = pending.start() else { panic!("cannot start") };
    t.server.pane_failed(id, &process, &why);
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Restoring);
    assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
    t.until("the source starts without waiting for a nonexistent process to exit", |t| {
        t.server.panes[&1].has_printed()
    });
}

#[test]
fn enter_after_a_synchronous_recovery_failure_resumes_the_source_conversation() {
    for undo in [false, true] {
        let mut t = SwitchTest::new(if undo { "undo-recovery-id" } else { "switch-recovery-id" });
        let switch = t.server.switches.get_mut(&1).unwrap();
        switch.from.agent = "codex".into();
        switch.to.agent = "codex".into();
        switch.undo = undo;
        let (from, to) = (switch.from.clone(), switch.to.clone());
        t.server.accounts = Accounts::empty();
        for (a, session) in [(&from, "source"), (&to, "destination")] {
            t.server.accounts.add(a.clone()).unwrap();
            let dir = PathBuf::from(&a.home).join("sessions/2026/10/13");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("rollout-today-{session}.jsonl")),
                format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\"}}}}\n"),
            )
            .unwrap();
        }
        let program = t.dir.join("codex");
        let script = "#!/bin/sh\necho \"$*\" > \"$CODEX_HOME/args\"\necho READY\nread -r line\nexit 23\n";
        crate::testing::write_script(&program, script);
        let mut original = t.server.agent_spec(&from, Some(t.dir.clone()), Some("source"), &[]).unwrap();
        original.command[0] = program.display().to_string();
        let mut destination = t.server.agent_spec(&to, Some(t.dir.clone()), Some("destination"), &[]).unwrap();
        destination.command[0] = program.display().to_string();
        t.spawn(PaneSpec { command: vec!["sh".into(), "-c".into(), "exit".into()], ..Default::default() });
        t.until("the initial process ends", |t| t.server.panes[&1].exit_code.is_some());
        let switch = t.server.switches.get_mut(&1).unwrap();
        switch.original = original;
        switch.stage = SwitchStage::Starting;
        t.server.start_switch_spec(1, destination).unwrap();
        assert_eq!(t.server.panes[&1].agent.session_id.as_deref(), Some("destination"));
        t.until("the destination runs", |t| crate::testing::read_complete_lines(t.dir.join("to/args")).is_some());
        std::fs::remove_file(&program).unwrap();
        t.server.panes.get_mut(&1).unwrap().write_input(b"exit\n".to_vec());
        t.until("recovery fails synchronously", |t| t.server.switches.is_empty());
        let pane = &t.server.panes[&1];
        assert!(pane.exit_code.is_some());
        assert_eq!(pane.spec.session.as_deref(), Some("source"));
        assert_eq!(pane.agent.session_id.as_deref(), Some("source"));
        assert_eq!(pane.agent.session_process, None);
        let (retry, exact) = t.server.restart_spec(1).unwrap();
        assert!(exact);
        assert!(retry.command.windows(2).any(|args| args == ["resume", "source"]));

        crate::testing::write_script(&program, script);
        let (client, _peer) = super::super::tests::client(&mut t.server);
        let tab = &mut t.server.workspaces.last_mut().unwrap().tabs[0];
        tab.root = Some(crate::layout::Node::Leaf(1));
        tab.set_focus(1);
        t.server.client_input(client, b"\r");
        let mut args = None;
        t.until("Enter resumes the source", |t| {
            args = crate::testing::read_complete_lines(t.dir.join("from/args"));
            args.is_some()
        });
        assert_eq!(args.unwrap().trim(), "resume source");
        assert_eq!(t.server.panes[&1].spec.account_id, Some(from.id));
    }
}
#[test]
fn removing_the_destination_after_the_stop_resumes_the_source() {
    let mut t = SwitchTest::new("removed-after-stop");
    t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Stopping;
    t.server.accounts.remove("claude", "to");
    t.server.finish_switch(1);
    assert_eq!(t.server.switches[&1].stage, SwitchStage::Restoring);
    assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
    assert!(t.server.panes[&1].spec.account_undo.is_none());
}
#[test]
fn a_process_that_changes_during_verification_offers_another_pane() {
    let mut t = SwitchTest::new("process-changed-during-check");
    let (client, _peer) = super::super::tests::client(&mut t.server);
    let s = t.server.switches.get_mut(&1).unwrap();
    s.client = Some(client);
    s.observed = Some(ObservedAgent {
        pid: u32::MAX,
        started: 0,
        command: vec!["claude".into()],
        account: s.from.clone(),
        session: "session".into(),
        cwd: t.dir.clone(),
        path: String::new(),
    });
    t.checked(Status::SignedIn);
    let Mode::Picker(p) = &t.server.clients[&client].mode else { panic!("a safe next step") };
    assert_eq!(p.title, "Can't switch this process.");
    assert!(matches!(p.items[0].action, PickAction::OpenAnotherAccount(1)));
    assert!(t.server.switches.is_empty());
    t.unchanged();
}
