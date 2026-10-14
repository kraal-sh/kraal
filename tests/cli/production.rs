use super::support::PATIENCE;
#[cfg(target_os = "linux")]
use super::support::RawClient;
use crate::common::Env;
#[cfg(target_os = "linux")]
use crate::common::{wait_until, write_script};
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
use std::time::{Duration, Instant};

/// A directory a quiet pane is in that becomes a Terraform one (`terraform init` run from
/// elsewhere) is guarded as soon as it is: the change is told, not looked for now and then.
#[test]
fn a_directory_that_becomes_a_terraform_one_is_guarded() {
    let env = Env::new("tfnew");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(&tf).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"t\" cwd={:?}\n}}\n", tf.display().to_string()));
    env.run(&["open", "w"]);
    env.wait_for_text("t", "$");
    let production = || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let t = panes.as_array().unwrap().iter().find(|p| p["name"] == "t").cloned().unwrap();
        t["production"].as_str().map(String::from)
    };
    // Known not to be a Terraform directory, and quiet.
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(production(), None);
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    let start = Instant::now();
    while production().is_none() {
        assert!(start.elapsed() < PATIENCE, "never seen as production");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(production().as_deref(), Some("Terraform workspace prod"));
    let out = env.try_run(&["send", "-p", "t", "echo", "hi"]);
    assert!(!out.status.success(), "sent to production");
}

#[test]
fn production_panes_refuse_unasked_cli_input() {
    let env = Env::new("prodguard");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh"]);
    // The Terraform workspace is seen by the directory scan, every few seconds.
    let start = Instant::now();
    loop {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let ops = panes.as_array().unwrap().iter().find(|p| p["name"] == "ops").cloned().unwrap();
        if ops["production"].is_string() {
            assert_eq!(ops["production"], "Terraform workspace prod");
            assert_eq!(ops["contexts"][0], "tf:prod");
            break;
        }
        assert!(start.elapsed() < PATIENCE, "never seen as production: {ops}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = env.try_run(&["send", "-p", "ops", "echo", "hi"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is production"), "{}", String::from_utf8_lossy(&out.stderr));
    env.run(&["send", "-p", "ops", "--production", "--enter", "echo", "SENT-ON-PURPOSE"]);
    env.wait_for_text("ops", "SENT-ON-PURPOSE");
    env.run(&["send", "-p", "main", "--enter", "echo", "not-production"]);
}

/// A tool that acts goes by what the pane's kubeconfig says when it starts: switched to
/// production a moment before (`kubectx`), it asks first.
#[cfg(target_os = "linux")]
#[test]
fn a_tool_asks_when_the_kubeconfig_just_switched_to_production() {
    let env = Env::new("kubenow");
    let kube = env.root.join("kube.yaml");
    let config = |current: &str| {
        format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: {current}\ncontexts:\n\
             - name: dev\n  context: {{cluster: c}}\n- name: prod-eks\n  context: {{cluster: c}}\n"
        )
    };
    std::fs::write(&kube, config("dev")).unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("kubectl"), "#!/bin/sh\nsleep 1000\n");
    let marked = env.root.join("marked");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[tools.mark]\ncommand = \"touch {}\"\nkey = \"m\"\n", marked.display()));
    std::fs::write(&cfg, text).unwrap();
    // A pane where kubectl runs on that kubeconfig.
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane name=\"k\" command=\"env\" {{ args \"KUBECONFIG={}\" \"{}\" \"get\" \"pods\"; }}\n}}\n",
            kube.display(),
            bin.join("kubectl").display()
        ),
    );
    env.run(&["open", "w"]);
    let start = Instant::now();
    loop {
        let panes = env.run(&["panes", "--json"]);
        if panes.contains("dev") {
            break;
        }
        assert!(start.elapsed() < PATIENCE, "the pane's context is read: {panes}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut client = RawClient::attach(&env, "w").traced();
    client.first_frame();
    // Switched to production, then the tool at once: before the next look at the file.
    std::fs::write(&kube, config("prod-eks")).unwrap();
    client.send(b"\x02Rm");
    client.wait_for_row("Run mark where w › k is?");
    assert!(!marked.exists(), "the tool ran on production without asking");
    // It asked: yes runs it.
    client.send(b"y");
    let start = Instant::now();
    while !marked.exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "the tool runs once allowed");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A pane that runs kubectl on `kube` for 2 s, then is a shell: its kubeconfig is known, and
/// what is typed there shows.
#[cfg(target_os = "linux")]
fn kube_shell_pane(env: &Env, name: &str, kube: &Path) -> String {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("kubectl"), "#!/bin/sh\nsleep 2\nexport PS1='$ '\nexec sh\n");
    format!(
        "pane name=\"{name}\" command=\"env\" {{ args \"KUBECONFIG={}\" \"{}\" \"get\" \"pods\"; }}",
        kube.display(),
        bin.join("kubectl").display()
    )
}

#[cfg(target_os = "linux")]
fn kubeconfig_text(current: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\ncurrent-context: {current}\ncontexts:\n\
         - name: dev\n  context: {{cluster: c}}\n- name: prod-x\n  context: {{cluster: c}}\n"
    )
}

/// Wait until the panes say a context: the pane's kubeconfig was read.
#[cfg(target_os = "linux")]
fn wait_for_context(env: &Env, context: &str) {
    let start = Instant::now();
    loop {
        let panes = env.run(&["panes", "--json"]);
        if panes.contains(context) {
            return;
        }
        assert!(start.elapsed() < PATIENCE, "the pane's context is read: {panes}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait until the server reads `file` again, a context file that was just changed and whose reads
/// `<file>.slow` makes last: the next scan starts that read (within 3.4 s, later on a loaded
/// machine), and what is typed from here on finds it under way.
#[cfg(target_os = "linux")]
fn wait_for_slow_read(file: &Path) {
    let reading = slow_read_marker(file);
    wait_until(PATIENCE, "the context file is never read again", || reading.exists());
}

/// Wait until the slow part of the read ends; its result must still be observed in the server.
#[cfg(target_os = "linux")]
fn wait_for_slow_read_to_end(file: &Path) {
    let reading = slow_read_marker(file);
    wait_until(PATIENCE, "the read of the context file never ended", || !reading.exists());
}

/// What sits next to `file` while the server reads it slowly.
#[cfg(target_os = "linux")]
fn slow_read_marker(file: &Path) -> PathBuf {
    let mut reading = file.as_os_str().to_owned();
    reading.push(".reading");
    PathBuf::from(reading)
}

/// A tool that acts waits for the pane's kubeconfig to be read again, in a list that keeps
/// the keys typed meanwhile (Esc gives up: nothing starts), and runs where it was asked for or
/// not at all.
#[cfg(target_os = "linux")]
#[test]
fn a_tool_waits_for_its_check_and_runs_only_where_it_was_asked() {
    let env = Env::new("toolwait");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    let marked = env.root.join("marked");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[tools.mark]\ncommand = \"touch {}\"\nkey = \"m\"\n", marked.display()));
    std::fs::write(&cfg, text).unwrap();
    env.workspace("w", &format!("layout {{\n    {}\n}}\n", kube_shell_pane(&env, "k", &kube)));
    // Reads of the kubeconfig take 3 s while `kube.yaml.slow` exists, as on a stalled disk.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "3000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("k", "$");
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    // The same content, a new modification time: the next read is a real, slow one.
    let touch = || std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();

    // Given up while it waits: the keys stay in the list, and nothing starts.
    touch();
    typed(b"\x02Rm");
    std::thread::sleep(Duration::from_millis(400));
    typed(b"zz");
    std::thread::sleep(Duration::from_millis(300));
    typed(b"\x1b");
    std::thread::sleep(Duration::from_millis(4000));
    assert!(!marked.exists(), "a tool given up on started anyway");
    let screen = env.run(&["read", "-p", "k"]);
    assert!(!screen.contains("zz"), "keys typed while the tool waited reached the pane:\n{screen}");

    // The pane moves while the tool waits: it does not start somewhere else.
    touch();
    typed(b"\x02Rm");
    std::thread::sleep(Duration::from_millis(400));
    env.run(&["send", "-p", "k", "-e", "cd /tmp"]);
    std::thread::sleep(Duration::from_millis(4500));
    assert!(!marked.exists(), "the tool started in a place it was not asked for");

    // Otherwise it runs once the kubeconfig has been read.
    touch();
    typed(b"\x02Rm");
    let start = Instant::now();
    while !marked.exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "the tool runs after the check");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// Keys broadcast to a pane whose kubeconfig is being read again wait for the read: once it
/// says production, they are asked about before reaching it.
#[cfg(target_os = "linux")]
#[test]
fn broadcast_waits_while_a_pane_context_is_read_again() {
    let env = Env::new("bcastread");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    // A real read of the kubeconfig takes 4.8 s while `kube.yaml.slow` exists: less than the
    // 5 s after which it would count as unknown.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    let mut client = RawClient::attach(&env, "w").traced();
    client.wait_for_row("$");
    env.run(&["focus", "a"]);
    client.send(b"\x02b");
    client.wait_for_row("BROADCAST");
    // `kubectx prod-x`: the next scan (within 3.4 s) starts a read that lasts 4.8 s.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube);
    client.send(b"echo hello-bcast");
    std::thread::sleep(Duration::from_millis(600));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("hello-bcast"), "typed while {pane}'s context was read:\n{screen}");
    }
    // The read ends, says production: the keys are asked about, not sent.
    wait_for_slow_read_to_end(&kube);
    client.wait_for_row("Typing is synced to production:");
    let screen = env.run(&["read", "-p", "b"]);
    assert!(!screen.contains("hello-bcast"), "keys reached a production pane unasked:\n{screen}");
    client.send(b"y");
    env.wait_for_text("b", "echo hello-bcast");
    env.wait_for_text("a", "echo hello-bcast");
}

/// What is typed while a broadcast pane's context is read again goes as one: when the read
/// says production and the answer is no, none of it is sent, the Enter at its end included.
#[cfg(target_os = "linux")]
#[test]
fn a_no_drops_everything_typed_while_a_context_was_read() {
    let env = Env::new("heldno");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    let mut client = RawClient::attach(&env, "w").traced();
    client.wait_for_row("$");
    env.run(&["focus", "a"]);
    client.send(b"\x02b");
    client.wait_for_row("BROADCAST");
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube);
    // A whole command, Enter included, typed while the read goes on.
    client.send(b" echo should-not-run\r");
    wait_for_slow_read_to_end(&kube);
    // The read said production: the question is about all of it. No.
    client.wait_for_row("Typing is synced to production:");
    client.send(b"n");
    client.wait_for_row("sync off · nothing was sent");
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("should-not-run"), "sent to {pane} after a no:\n{screen}");
    }
}

/// A yes is for the panes the question named, as they were then: given while one of them has
/// its context read again, it waits for the read, and nothing goes if the read changed it.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_waits_for_a_context_read_and_holds_only_if_nothing_changed() {
    let env = Env::new("heldyes");
    let kube_b = env.root.join("kube-b.yaml");
    let kube_c = env.root.join("kube-c.yaml");
    std::fs::write(&kube_b, kubeconfig_text("prod-x")).unwrap();
    std::fs::write(&kube_c, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube_b),
            kube_shell_pane(&env, "c", &kube_c)
        ),
    );
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "prod-x");
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    env.wait_for_text("c", "$");
    // Leave room for the full rejection message beside the broadcast indicator and tab.
    let mut client = RawClient::attach_sized(&env, Some("w"), (200, 40)).traced();
    client.wait_for_row("$");
    env.run(&["focus", "a"]);
    client.send(b"\x02b");
    client.wait_for_row("BROADCAST");
    // b is production: the paste is asked about.
    client.send(b"\x1b[200~echo answered-early\x1b[201~");
    client.wait_for_row("Typing is synced to production:");
    // Meanwhile c becomes production, and its read lasts.
    std::fs::write(env.root.join("kube-c.yaml.slow"), "").unwrap();
    std::fs::write(&kube_c, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube_c);
    // Yes, before c's read ends: it was not production when asked.
    client.send(b"y");
    client.wait_for_rows("the answer was taken", |rows| {
        !rows.iter().any(|row| row.contains("Typing is synced to production:"))
    });
    assert!(slow_read_marker(&kube_c).exists(), "c's read ended before the answer was taken");
    wait_for_slow_read_to_end(&kube_c);
    client.wait_for_row("a pane changed production since the question: nothing was sent");
    for pane in ["a", "b", "c"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("answered-early"), "sent to {pane} though c changed after the question:\n{screen}");
    }
    // Asked again (c is production now), answered during a read that changes nothing: it goes,
    // once the read has ended.
    client.send(b"\x1b[200~echo answered-again\x1b[201~");
    client.wait_for_row("Typing is synced to production:");
    std::fs::write(&kube_c, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube_c);
    client.send(b"y");
    client.wait_for_rows("the answer was taken", |rows| {
        !rows.iter().any(|row| row.contains("Typing is synced to production:"))
    });
    for pane in ["a", "b", "c"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("answered-again"), "sent to {pane} before c's read ended:\n{screen}");
    }
    assert!(slow_read_marker(&kube_c).exists(), "c's read ended before the answer was checked");
    wait_for_slow_read_to_end(&kube_c);
    for pane in ["a", "b", "c"] {
        let screen = env.wait_for_text(pane, "echo answered-again");
        assert_eq!(screen.matches("echo answered-again").count(), 1, "sent more than once to {pane}:\n{screen}");
    }
}

/// What is typed in a tab while a broadcast pane's context is read again goes to that tab's
/// panes once the read ends, even when another tab is shown by then.
#[cfg(target_os = "linux")]
#[test]
fn input_that_waited_goes_where_it_was_typed() {
    let env = Env::new("heldtab");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    tab name=\"one\" {{\n        pane split_direction=\"vertical\" {{\n            pane name=\"a\"\n            {}\n        }}\n    }}\n    tab name=\"two\" {{\n        pane split_direction=\"vertical\" {{\n            pane name=\"c\"\n            pane name=\"d\"\n        }}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    env.run(&["send", "-p", "c", "--enter", "echo TAB-TWO"]);
    env.wait_for_text("c", "TAB-TWO");
    let mut client = RawClient::attach(&env, "w").traced();
    client.wait_for_row("$");
    env.run(&["focus", "a"]);
    client.send(b"\x02b");
    client.wait_for_row("BROADCAST");
    // The same context written again: a read that changes nothing, and lasts.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    wait_for_slow_read(&kube);
    client.send(b"echo typed-in-one");
    // Tab two is shown before the read ends.
    client.send(b"\x022");
    client.wait_for_row("TAB-TWO");
    assert!(slow_read_marker(&kube).exists(), "the read ended before the tab changed");
    wait_for_slow_read_to_end(&kube);
    env.wait_for_text("a", "echo typed-in-one");
    env.wait_for_text("b", "echo typed-in-one");
    for pane in ["c", "d"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("typed-in-one"), "reached {pane} in the tab shown later:\n{screen}");
    }
}

/// A yes given while a pane's kubeconfig is read again and the read does not answer is not
/// taken for the context known before the read (the file may say another one now): nothing goes.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_during_a_read_that_does_not_answer_sends_nothing() {
    let env = Env::new("heldstall");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    // A read of the kubeconfig takes 20 s while `kube.yaml.slow` exists: after 5 s it does not
    // answer.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "20000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "prod-x");
    env.wait_for_text("b", "$");
    // Leave room for the full rejection message beside the broadcast indicator and tab.
    let mut client = RawClient::attach_sized(&env, Some("w"), (200, 40)).traced();
    client.wait_for_row("$");
    env.run(&["focus", "a"]);
    client.send(b"\x02b");
    client.wait_for_row("BROADCAST");
    // b is production: the paste is asked about.
    client.send(b"\x1b[200~echo stalled-yes\x1b[201~");
    client.wait_for_row("Typing is synced to production:");
    // Meanwhile b's kubeconfig changes, and the read of it does not answer.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    wait_for_slow_read(&kube);
    // Later scans must not start another slow read after this one ends.
    std::fs::remove_file(env.root.join("kube.yaml.slow")).unwrap();
    wait_until(PATIENCE, "the context never became unknown", || {
        pane_json(&env, "b")["production"].as_str().is_some_and(|why| why.contains("does not answer"))
    });
    assert!(slow_read_marker(&kube).exists(), "the stalled read ended before the answer");
    client.send(b"y");
    client.wait_for_row("a pane changed production since the question: nothing was sent");
    wait_for_slow_read_to_end(&kube);
    wait_for_contexts(&env, "b", &["⎈ dev"], None);
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("stalled-yes"), "sent to {pane} on a yes for what the file said before:\n{screen}");
    }
}

/// As for a kubeconfig: once the directory a Terraform run worked on is read again and does not
/// answer, the workspace read before no longer counts, production or not. A yes given for it
/// sends nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_during_a_terraform_read_that_does_not_answer_sends_nothing() {
    let env = Env::new("tfstall");
    let infra = env.root.join("infra");
    std::fs::create_dir_all(infra.join(".terraform")).unwrap();
    let environment = infra.join(".terraform/environment");
    std::fs::write(&environment, "prod-a\n").unwrap();
    // A Terraform run, seen while it runs: the pane's workspace is the one of the directory it
    // ran in, read off the event loop.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("terraform"), "#!/bin/sh\nsleep 2\nexport PS1='$ '\nexec sh\n");
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        \
             pane name=\"b\" cwd=\"{}\" command=\"{}\" {{ args \"plan\"; }}\n    }}\n}}\n",
            infra.display(),
            bin.join("terraform").display()
        ),
    );
    // A read of the environment file takes 20 s while `environment.slow` exists: after 5 s it
    // does not answer.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "20000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    // Leave room for the full rejection message beside the broadcast indicator and tab.
    let mut client = RawClient::attach_sized(&env, Some("w"), (200, 40)).traced();
    client.wait_for_row("$");
    env.run(&["focus", "a"]);
    client.send(b"\x02b");
    client.wait_for_row("BROADCAST");
    // b is production: the paste is asked about.
    client.send(b"\x1b[200~echo stalled-tf-yes\x1b[201~");
    client.wait_for_row("Typing is synced to production:");
    // Meanwhile b's workspace changes, and the read of it does not answer.
    std::fs::write(infra.join(".terraform/environment.slow"), "").unwrap();
    std::fs::write(&environment, "prod-b\n").unwrap();
    wait_for_slow_read(&environment);
    // Later scans must not start another slow read after this one ends.
    std::fs::remove_file(infra.join(".terraform/environment.slow")).unwrap();
    wait_until(PATIENCE, "the context never became unknown", || {
        pane_json(&env, "b")["production"].as_str().is_some_and(|why| why.contains("does not answer"))
    });
    assert!(slow_read_marker(&environment).exists(), "the stalled read ended before the answer");
    client.send(b"y");
    client.wait_for_row("a pane changed production since the question: nothing was sent");
    wait_for_slow_read_to_end(&environment);
    wait_for_contexts(&env, "b", &["tf:prod-b"], Some("Terraform workspace prod-b"));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("stalled-tf-yes"), "sent to {pane} on a yes for the workspace before:\n{screen}");
    }
}

/// The same with no Terraform run seen: the workspace is that of the directory the pane is in.
/// Once a read of it does not answer, the workspace read before no longer counts, and a yes
/// given for it sends nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_during_a_read_of_the_directory_a_pane_is_in_that_does_not_answer_sends_nothing() {
    let env = Env::new("tfdirstall");
    let infra = env.root.join("infra");
    std::fs::create_dir_all(infra.join(".terraform")).unwrap();
    let environment = infra.join(".terraform/environment");
    std::fs::write(&environment, "prod-a\n").unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        \
             pane name=\"b\" cwd=\"{}\" command=\"sh\"\n    }}\n}}\n",
            infra.display()
        ),
    );
    // A read of the environment file takes 20 s while `environment.slow` exists: after 5 s it
    // does not answer.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "20000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    // Leave room for the full rejection message beside the broadcast indicator and tab.
    let mut client = RawClient::attach_sized(&env, Some("w"), (200, 40)).traced();
    client.wait_for_row("$");
    env.run(&["focus", "a"]);
    client.send(b"\x02b");
    client.wait_for_row("BROADCAST");
    // b is production: the paste is asked about.
    client.send(b"\x1b[200~echo stalled-dir-yes\x1b[201~");
    client.wait_for_row("Typing is synced to production:");
    // Meanwhile the directory's workspace changes, and the read of it does not answer.
    std::fs::write(infra.join(".terraform/environment.slow"), "").unwrap();
    std::fs::write(&environment, "prod-b\n").unwrap();
    wait_for_slow_read(&environment);
    // Later scans must not start another slow read after this one ends.
    std::fs::remove_file(infra.join(".terraform/environment.slow")).unwrap();
    wait_until(PATIENCE, "the context never became unknown", || {
        pane_json(&env, "b")["production"].as_str().is_some_and(|why| why.contains("does not answer"))
    });
    assert!(slow_read_marker(&environment).exists(), "the stalled read ended before the answer");
    client.send(b"y");
    client.wait_for_row("a pane changed production since the question: nothing was sent");
    wait_for_slow_read_to_end(&environment);
    wait_for_contexts(&env, "b", &["tf:prod-b"], Some("Terraform workspace prod-b"));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("stalled-dir-yes"), "sent to {pane} on a yes for the workspace before:\n{screen}");
    }
}

/// A directory known to hold a production Terraform workspace stays known after the pane leaves
/// it: coming back, input is guarded at once, not only after the next read.
#[cfg(target_os = "linux")]
#[test]
fn a_production_directory_left_and_come_back_to_is_still_guarded() {
    let env = Env::new("tfback");
    let infra = env.root.join("infra");
    std::fs::create_dir_all(infra.join(".terraform")).unwrap();
    std::fs::write(infra.join(".terraform/environment"), "prod-a\n").unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"b\" cwd=\"{}\" command=\"sh\"\n}}\n", infra.display()));
    // A read of the environment file takes ten minutes while `environment.slow` exists: longer
    // than this test waits for anything.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "600000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    let refused = |text: &str| !env.cmd(&["send", "-p", "b", "-e", text]).output().unwrap().status.success();
    assert!(refused("echo here"), "b is production");
    env.run(&["send", "-p", "b", "-e", "--production", "cd /"]);
    wait_for_contexts(&env, "b", &[], None);
    // A scan (every 3 s) passes while no pane is in the directory.
    std::thread::sleep(Duration::from_millis(4000));
    assert!(!refused("echo away"), "away from the directory, b is not production");
    // The directory cannot be read again in time from here on: what is said of it once the pane
    // is back is what was known, however long the pane takes to get there.
    std::fs::write(infra.join(".terraform/environment.slow"), "").unwrap();
    env.run(&["send", "-p", "b", "-e", &format!("cd {}", infra.display())]);
    wait_for_contexts(&env, "b", &["tf:prod-a"], Some("Terraform workspace prod-a"));
    assert!(refused("echo back"), "back in the directory, b is production again");
}

/// The pane named `name`, as `kraal panes --json` lists it.
#[cfg(target_os = "linux")]
fn pane_json(env: &Env, name: &str) -> serde_json::Value {
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    panes.as_array().unwrap().iter().find(|p| p["name"] == name).cloned().unwrap()
}

/// Wait until the pane named `name` shows `contexts` and is production for `why` (or not).
#[cfg(target_os = "linux")]
fn wait_for_contexts(env: &Env, name: &str, contexts: &[&str], why: Option<&str>) {
    let start = Instant::now();
    loop {
        let pane = pane_json(env, name);
        if pane["contexts"] == serde_json::json!(contexts) && pane["production"] == serde_json::json!(why) {
            return;
        }
        assert!(start.elapsed() < PATIENCE, "never {contexts:?} ({why:?}): {pane}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A shell on a server with what `kraal integration show` prints in its startup file reports
/// what it works on: its pane shows it, and is guarded like a local one, until the session ends.
/// The `ssh` here is `script`, which gives the shell a terminal of its own and relays it as ssh
/// does; the shell finds kraal by asking it, through that relay.
#[cfg(target_os = "linux")]
fn contexts_over_ssh_hold_until_the_session_ends(shell: &str) {
    let script = ["/usr/bin/script", "/bin/script"].into_iter().map(Path::new).find(|p| p.exists());
    let Some(script) = script else { return };
    if Command::new(shell).arg("-c").arg("true").output().is_err() {
        return;
    }
    let env = Env::new(&format!("sshctx-{shell}"));
    let config = env.root.join("config/kraal/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[servers]\nauto_add = false\n")).unwrap();
    // The server's home: a kubeconfig on production, and the shell integration.
    let remote = env.root.join("remote");
    std::fs::create_dir_all(remote.join(".kube")).unwrap();
    let kube = "contexts:\n- context:\n    namespace: payments\n  name: prod-eks\n- name: dev\n  context: {}\n\
        current-context: prod-eks\n";
    std::fs::write(remote.join(".kube/config"), kube).unwrap();
    std::fs::write(remote.join("dev.yaml"), kube.replace("current-context: prod-eks", "current-context: dev")).unwrap();
    let block = env.run(&["integration", "show", shell]);
    let r = remote.display();
    let start = match shell {
        "bash" => {
            std::fs::write(remote.join(".bashrc"), format!("PS1='remote$ '\n{block}\n")).unwrap();
            format!("bash --rcfile {r}/.bashrc -i")
        }
        "zsh" => {
            std::fs::write(remote.join(".zshrc"), format!("PS1='remote$ '\n{block}\n")).unwrap();
            format!("env ZDOTDIR={r} zsh --no-globalrcs -i")
        }
        _ => {
            let fish = remote.join(".config/fish");
            std::fs::create_dir_all(fish.join("conf.d")).unwrap();
            std::fs::write(fish.join("conf.d/kraal.fish"), format!("{block}\n")).unwrap();
            let prompt = "set -g fish_greeting\nfunction fish_prompt\n    printf 'remote$ '\nend\n";
            std::fs::write(fish.join("config.fish"), prompt).unwrap();
            format!("env XDG_CONFIG_HOME={r}/.config XDG_DATA_HOME={r}/.local/share fish -i")
        }
    };
    let export = |k: &str, v: &str| match shell {
        "fish" => format!("set -gx {k} {v}"),
        _ => format!("export {k}={v}"),
    };
    let login = env.root.join("login");
    write_script(
        &login,
        &format!(
            "#!/bin/sh\nexec env -i PATH=/usr/bin:/bin HOME={r} TERM=xterm-256color \
             SSH_CONNECTION='10.0.0.1 50000 10.0.0.5 22' {start}\n"
        ),
    );
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(script, bin.join("ssh")).unwrap();
    env.workspace("w", "layout {\n    pane name=\"ops\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("ops", "$");
    // `ssh -q -f -c <cipher> bastion`, as kraal reads it; `script` writes its log to ./bastion.
    let connect = format!(
        "cd {} && SHELL=/bin/sh {} -q -f -c {} bastion",
        env.root.display(),
        bin.join("ssh").display(),
        login.display()
    );
    env.run(&["send", "-p", "ops", "--enter", &connect]);
    wait_for_contexts(&env, "ops", &["⎈ prod-eks/payments"], Some("Kubernetes context prod-eks on bastion"));
    assert_eq!(pane_json(&env, "ops")["server"], "bastion");
    env.wait_for_text("ops", "remote$");
    let out = env.try_run(&["send", "-p", "ops", "--enter", "echo hi"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("is production"), "{}", String::from_utf8_lossy(&out.stderr));
    // Another kubeconfig on the server: the next prompt says so.
    env.run(&["send", "-p", "ops", "--production", "--enter", &export("KUBECONFIG", "$HOME/dev.yaml")]);
    wait_for_contexts(&env, "ops", &["⎈ dev"], None);
    // A command that runs and ends keeps it.
    env.run(&["send", "-p", "ops", "--enter", "true"]);
    env.run(&["send", "-p", "ops", "--enter", &export("AWS_PROFILE", "prd-admin")]);
    wait_for_contexts(&env, "ops", &["⎈ dev", "aws:prd-admin"], Some("AWS profile prd-admin on bastion"));
    // The session ends: what it reported goes with it.
    env.run(&["send", "-p", "ops", "--production", "--enter", "exit"]);
    wait_for_contexts(&env, "ops", &[], None);
    assert!(pane_json(&env, "ops")["server"].is_null());
    // A local shell's report is not taken: kraal reads local contexts itself.
    env.run(&["send", "-p", "ops", "--enter", "printf '\\033]7701;context;k8s=prod-x\\007'; echo printed"]);
    env.wait_for_text("ops", "printed");
    std::thread::sleep(Duration::from_millis(1000));
    wait_for_contexts(&env, "ops", &[], None);
}

#[cfg(target_os = "linux")]
#[test]
fn contexts_bash_reports_over_ssh_hold_until_the_session_ends() {
    contexts_over_ssh_hold_until_the_session_ends("bash");
}

#[cfg(target_os = "linux")]
#[test]
fn contexts_zsh_reports_over_ssh_hold_until_the_session_ends() {
    contexts_over_ssh_hold_until_the_session_ends("zsh");
}

/// fish 4 marks its prompts itself too: twice marked, a prompt keeps what was reported.
#[cfg(target_os = "linux")]
#[test]
fn contexts_fish_reports_over_ssh_hold_until_the_session_ends() {
    contexts_over_ssh_hold_until_the_session_ends("fish");
}

/// When a long paste turns out to go into production while its last pieces wait for a context
/// read, the rest of it does not go, and neither does what was typed after it: an Enter would
/// run the part of it that went.
#[cfg(target_os = "linux")]
#[test]
fn a_long_paste_cut_for_production_takes_the_enter_after_it_along() {
    let env = Env::new("heldpiece");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace("w", &format!("layout {{\n    {}\n}}\n", kube_shell_pane(&env, "k", &kube)));
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("k", "$");
    // What reaches the pane goes to a file, byte for byte.
    let got = env.root.join("got");
    env.run(&["send", "-p", "k", "-e", &format!("stty raw -echo; cat > {}", got.display())]);
    std::thread::sleep(Duration::from_millis(500));
    let mut client = RawClient::attach(&env, "w");
    std::thread::sleep(Duration::from_millis(500));
    // More than a paste held whole (8 MiB): it goes as it comes.
    let piece = vec![b'a'; 64 << 10];
    client.send(b"\x1b[200~");
    for _ in 0..(9 << 20) / piece.len() {
        client.send(&piece);
    }
    // `kubectx prod-x`: the next scan (within 3.4 s) starts a read that lasts 4.8 s, and the
    // rest of the paste, then an Enter, wait for it.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube);
    for _ in 0..16 {
        client.send(&piece);
    }
    client.send(b"\x1b[201~");
    client.send(b"\r");
    // The read ends and says production.
    std::thread::sleep(Duration::from_millis(6000));
    let bytes = std::fs::read(&got).unwrap();
    assert!(bytes.len() >= 8 << 20, "the start of the paste went: {} bytes", bytes.len());
    assert!(!bytes.contains(&b'\r'), "the Enter typed after a cut paste reached the pane");
    assert!(bytes.len() < (9 << 20) + 16 * piece.len(), "the rest of the paste went into production");
}
