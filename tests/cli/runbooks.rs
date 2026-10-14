#[cfg(target_os = "linux")]
use super::support::in_terminal;
use super::support::{RawClient, numbered, raw_request, wait_gone, without_trailing_spaces};
use crate::common::{Env, git_repo, wait_until, write_script};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

#[cfg(unix)]
#[test]
fn runbooks_run_locally_and_on_servers() {
    let env = Env::new("runbooks");
    // A fake ssh: runs the remote script here, and fails for the server "bad".
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("ssh"),
        "#!/bin/sh\nfor a; do dest=$cmd; cmd=$a; done\ncase $dest in *bad*) echo unreachable >&2; exit 255;; esac\nexec sh -c \"$cmd\"\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| env.cmd(args).env("PATH", &path).output().unwrap();
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"a\"\nhost = \"a.lan\"\ngroups = [\"web\"]\n\n[[server]]\nname = \"bad\"\nhost = \"bad.lan\"\ngroups = [\"web\"]\n\n[[server]]\nname = \"c\"\nhost = \"c.lan\"\ngroups = [\"web\"]\n",
    )
    .unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    assert!(run(&["open", "w"]).status.success());

    let out = run(&["runbook", "new", "greet"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let file = env.root.join("config/kraal/runbooks/greet.toml");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), file.display().to_string());
    std::fs::write(
        &file,
        "description = \"say hello\"\nsteps = [\"echo hello $WHO\", \"echo bye\"]\n[[param]]\nname = \"WHO\"\ndefault = \"world\"\n",
    )
    .unwrap();
    let list = String::from_utf8(run(&["runbook", "list"]).stdout).unwrap();
    assert!(list.contains("greet") && list.contains("say hello"), "{list}");

    let out = run(&["runbook", "run", "greet", "--dry-run", "-p", "WHO=kraal"]);
    let plan = String::from_utf8_lossy(&out.stdout);
    assert!(
        plan.contains("Run greet on this machine?")
            && plan.contains("WHO=kraal")
            && plan.contains("1. echo hello $WHO"),
        "{plan}"
    );
    let out = run(&["runbook", "run", "greet", "--yes", "--wait", "-p", "WHO=kraal"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("finished: 1 ok, 0 failed, 0 skipped"), "{stdout}");
    let log = stdout.rsplit("log ").next().unwrap().trim().to_string();
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(
        log_text.contains("param     WHO=kraal") && log_text.contains("hello kraal") && log_text.contains("1 ok"),
        "{log_text}"
    );

    // On a group, one server at a time: the failing one stops the run.
    std::fs::write(
        env.root.join("config/kraal/runbooks/check.toml"),
        "on = \"server\"\nsteps = [\"echo checked $KRAAL_SERVER\"]\n",
    )
    .unwrap();
    // A failing line inside a step fails it, even when later lines succeed.
    std::fs::write(
        env.root.join("config/kraal/runbooks/multi.toml"),
        "steps = [\"false\\necho after-false\", \"echo second-step\"]\n",
    )
    .unwrap();
    let out = run(&["runbook", "run", "multi", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success() && stdout.contains("finished: 0 ok, 1 failed"), "{stdout}");
    let log = std::fs::read_to_string(stdout.rsplit("log ").next().unwrap().trim()).unwrap();
    let output = log.split("\n--- ").nth(1).unwrap_or_default();
    assert!(output.contains("step 1 failed"), "{log}");
    assert!(!output.contains("after-false") && !output.contains("second-step"), "{log}");

    let out = run(&["runbook", "run", "check", "--yes"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--on"), "servers must be named");
    let out = run(&["runbook", "run", "check", "--on", "@web", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains("3 servers (a, bad, c), one at a time, stopping at the first failure"), "{stdout}");
    assert!(stdout.contains("finished: 1 ok, 1 failed, 1 skipped"), "{stdout}");
    let screen = String::from_utf8(run(&["read", "-p", "a"]).stdout).unwrap();
    assert!(screen.contains("checked a"), "{screen}");
    let screen = String::from_utf8(run(&["read", "-p", "c (skipped)"]).stdout).unwrap();
    assert!(screen.contains("skipped: an earlier target failed") && !screen.contains("checked c"), "{screen}");

    // All at once: one fails at once while the others still run. The run must wait for them,
    // not spin (this once overflowed the server's stack).
    std::fs::write(
        env.root.join("config/kraal/runbooks/wide.toml"),
        "on = \"server\"\nrolling = 0\nsteps = [\"sleep 0.5\", \"echo wide $KRAAL_SERVER\"]\n",
    )
    .unwrap();
    let out = run(&["runbook", "run", "wide", "--on", "@web", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("all at once"), "{stdout}");
    assert!(stdout.contains("finished: 2 ok, 1 failed, 0 skipped"), "{stdout}");
    assert!(run(&["ls"]).status.success(), "the server is still alive");

    // A target over its timeout is stopped, even when it ignores SIGTERM.
    std::fs::write(
        env.root.join("config/kraal/runbooks/slow.toml"),
        "timeout = \"1\"\nsteps = [\"trap '' TERM; sleep 60\"]\n",
    )
    .unwrap();
    let start = Instant::now();
    let out = run(&["runbook", "run", "slow", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("finished: 0 ok, 1 failed"), "{stdout}");
    assert!(start.elapsed() < Duration::from_secs(20), "stopped by SIGKILL after the grace period");
    let log = std::fs::read_to_string(stdout.rsplit("log ").next().unwrap().trim()).unwrap();
    assert!(log.contains("over its timeout") && log.contains("timed out, stopped"), "{log}");

    // What a timed-out target left running in its process group is stopped too, before the
    // target counts as timed out.
    let pidfile = env.root.join("left.pid");
    std::fs::write(
        env.root.join("config/kraal/runbooks/leaves.toml"),
        format!(
            "timeout = \"1\"\nsteps = [\"(trap '' TERM HUP; exec sleep 60) & echo $! > {}; wait\"]\n",
            pidfile.display()
        ),
    )
    .unwrap();
    let start = Instant::now();
    let out = run(&["runbook", "run", "leaves", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("finished: 0 ok, 1 failed"), "{stdout}");
    assert!(start.elapsed() < Duration::from_secs(20));
    let left: libc::pid_t = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    // SAFETY: signal 0 only checks that the process exists.
    assert_ne!(unsafe { libc::kill(left, 0) }, 0, "the background sleep {left} still runs");

    // The log has all of a target's output, not only what its pane still shows.
    std::fs::write(env.root.join("config/kraal/runbooks/long.toml"), "steps = [\"seq 1 3000\"]\n").unwrap();
    let out = run(&["runbook", "run", "long", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let log = std::fs::read_to_string(stdout.rsplit("log ").next().unwrap().trim()).unwrap();
    assert!(log.contains("\n1\n2\n3\n") && log.contains("\n3000\n"), "{log}");

    // A server that dies during a run: the next one says so in the run's log.
    std::fs::write(env.root.join("config/kraal/runbooks/cut.toml"), "steps = [\"echo cut-started\", \"sleep 60\"]\n")
        .unwrap();
    assert!(run(&["runbook", "run", "cut", "--yes"]).status.success());
    let runs = env.root.join("state/kraal/runs");
    let log = std::fs::read_dir(&runs)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.to_string_lossy().contains("-cut-") && p.extension().is_some_and(|x| x == "log"))
        .expect("the run's log");
    // Its output reaches the disk while it runs.
    let start = Instant::now();
    while !std::fs::read_to_string(log.with_extension("0.out")).unwrap_or_default().contains("cut-started") {
        assert!(start.elapsed() < Duration::from_secs(10), "no output streamed");
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = env.server_pid().expect("the server's pid");
    // SAFETY: plain syscall.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_gone(&PathBuf::from(format!("/proc/{pid}")));
    let _ = std::fs::remove_file(env.root.join("run/kraal/runbooks.sock"));
    // Killed while it wrote its run ledger, it would leave the temporary file of that write.
    std::fs::write(runs.join(format!("..active-runbooks.json.{pid}.38.tmp")), "[]").unwrap();
    env.run(&["open", "w"]);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("cut-started") && text.contains("interrupted") && text.contains("unfinished: this machine"),
        "{text}"
    );
    // The restarted server may be writing its run ledger (a temporary file, then a rename).
    let start = Instant::now();
    loop {
        let left: Vec<_> = std::fs::read_dir(&runs).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        if left.iter().all(|n| n.to_string_lossy().ends_with(".log")) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{left:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `kraal runbook run` takes for a parameter what the list of runbooks would: one of its
/// choices when it has some, written in its file or listed by its command, and anything
/// otherwise. A refusal names the choices and comes in place of the plan, so `--dry-run` fails
/// too. The command of a repository runbook nobody approved does not run: its plan says which
/// value waits, its run is refused, and once the runbook is approved a run of that plan holds
/// the value to the list.
#[test]
fn a_runbook_value_is_one_its_question_offers() {
    let env = Env::new("rbchoice");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let runbooks = env.root.join("config/kraal/runbooks");
    std::fs::create_dir_all(&runbooks).unwrap();
    let step = format!("steps = [\"echo $V > {}\"]\n", ran.display());
    let write = |name: &str, param: &str| {
        let text = format!("{step}[[param]]\nname = \"V\"\n{param}");
        std::fs::write(runbooks.join(format!("{name}.toml")), text).unwrap();
    };
    write("site", "choices = [\"shop\", \"blog\"]\ndefault = \"shop\"\n");
    write("odd", "choices = [\"shop\", \"blog\"]\ndefault = \"docs\"\n");
    write("tag", &format!("choices_command = \"echo x >> {}; echo v1; echo v2\"\n", listed.display()));
    std::fs::write(
        runbooks.join("flaky.toml"),
        "steps = [\"true\"]\n[[param]]\nname = \"A\"\nchoices_command = \"echo no >&2; exit 3\"\n[[param]]\nname = \"B\"\nchoices_command = \"true\"\n",
    )
    .unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // Whether it succeeded, what it printed, and its error.
    let kraal = |dir: &Path, args: &[&str]| {
        let mut c = env.cmd(&[&["runbook", "run"], args].concat());
        let out = c.current_dir(dir).stdin(std::process::Stdio::null()).output().unwrap();
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        (out.status.success(), text(&out.stdout), text(&out.stderr))
    };

    // Choices written in the file: no plan for another value, and nothing runs.
    let (ok, plan, why) = kraal(&env.root, &["site", "--dry-run", "-p", "V=nope"]);
    assert!(!ok && plan.is_empty(), "{plan}");
    assert!(why.contains("V=nope is not one of its choices: shop, blog"), "{why}");
    let (ok, _, why) = kraal(&env.root, &["site", "--yes", "--wait", "-p", "V=nope"]);
    assert!(!ok && why.contains("V=nope is not one of its choices") && !ran.exists(), "{why}");
    for (args, value) in [(&["-p", "V=blog"][..], "V=blog"), (&[], "V=shop")] {
        let (ok, plan, why) = kraal(&env.root, &[&["site", "--dry-run"], args].concat());
        assert!(ok && plan.contains(value), "{args:?}: {plan}{why}");
    }
    // A default is a value like another.
    let (ok, _, why) = kraal(&env.root, &["odd", "--dry-run"]);
    assert!(!ok && why.contains("V=docs is not one of its choices: shop, blog"), "{why}");

    // Choices listed by a command, once for a plan: its run does not list them again.
    let (ok, _, why) = kraal(&env.root, &["tag", "--dry-run", "-p", "V=v3"]);
    assert!(!ok && why.contains("V=v3 is not one of its choices: v1, v2"), "{why}");
    let (ok, said, why) = kraal(&env.root, &["tag", "--yes", "--wait", "-p", "V=v2"]);
    assert!(ok && said.contains("finished: 1 ok"), "{said}{why}");
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "v2\n");
    assert_eq!(std::fs::read_to_string(&listed).unwrap(), "x\nx\n");
    // A command that fails, or lists nothing, leaves the question a field: the plan says which
    // value was not checked.
    let (ok, plan, why) = kraal(&env.root, &["flaky", "--dry-run", "-p", "A=any", "-p", "B=thing"]);
    assert!(ok, "{why}");
    assert!(plan.contains("A is taken as given: \"echo no >&2; exit 3\" failed: no"), "{plan}");
    assert!(!plan.contains("B is taken"), "{plan}");

    // A repository's runbook, not approved: its command does not run. The plan says so, and a
    // run is refused for the approval it lacks.
    let repo = env.root.join("app");
    let deploy =
        format!("{step}[[param]]\nname = \"V\"\nchoices_command = \"echo x >> {}; echo v1\"\n", listed.display());
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &deploy)]);
    let file = repo.join(".kraal/runbooks/deploy.toml");
    for left in [&ran, &listed] {
        std::fs::remove_file(left).unwrap();
    }
    // What any program can send: a plan asked for, then run by its token.
    let plan = |value: &str| {
        let params = serde_json::json!([["V", value]]);
        let request =
            serde_json::json!({ "cmd": "runbook_plan", "path": file, "target": null, "params": params, "pane": null });
        let plan = raw_request(&env, request);
        plan["Ok"]["data"]["token"].as_str().unwrap_or_else(|| panic!("no plan: {plan}")).to_string()
    };
    let run = |token: &str| raw_request(&env, serde_json::json!({ "cmd": "runbook_run", "token": token }));
    let (ok, shown, why) = kraal(&repo, &["deploy", "--dry-run", "-p", "V=nope"]);
    assert!(ok, "{why}");
    assert!(shown.contains("V is taken as given: its choices are listed once the runbook is approved"), "{shown}");
    let answer = run(&plan("v1"));
    assert!(answer["Err"].as_str().is_some_and(|e| e.contains("is not approved")), "{answer}");
    let (other, known) = (plan("nope"), plan("v1"));
    assert!(!listed.exists() && !ran.exists());
    // A refusal quotes the choices its file gives: a terminal acts on none of it.
    let erasing = "steps = [\"true\"]\n[[param]]\nname = \"V\"\nchoices = [\"a\\u001b[2J\"]\n";
    std::fs::write(repo.join(".kraal/runbooks/pick.toml"), erasing).unwrap();
    let (ok, _, why) = kraal(&repo, &["pick", "--dry-run", "-p", "V=b"]);
    assert!(!ok && why.contains("V=b is not one of its choices: a\\u{1b}[2J") && !why.contains('\x1b'), "{why:?}");

    // Approved: its list holds from now on, for the plans made before too.
    let trust = env.cmd(&["runbook", "trust", "deploy"]).current_dir(&repo).output().unwrap();
    assert!(trust.status.success(), "{}", String::from_utf8_lossy(&trust.stderr));
    let (ok, _, why) = kraal(&repo, &["deploy", "--dry-run", "-p", "V=nope"]);
    assert!(!ok && why.contains("V=nope is not one of its choices: v1"), "{why}");
    let answer = run(&other);
    assert!(answer["Err"].as_str().is_some_and(|e| e.contains("V=nope is not one of its choices: v1")), "{answer}");
    assert!(!ran.exists());
    let answer = run(&known);
    assert!(answer["Ok"].is_object(), "{answer}");
    wait_until(Duration::from_secs(10), "the plan made before the approval never ran", || {
        std::fs::read_to_string(&ran).is_ok_and(|said| said == "v1\n")
    });
    assert_eq!(std::fs::read_to_string(&listed).unwrap(), "x\nx\nx\n");
}

/// Keys typed while the runbooks are being listed go to the list, not to the shell under it.
#[cfg(target_os = "linux")]
#[test]
fn keys_typed_while_runbooks_load_stay_in_the_list() {
    let env = Env::new("rbkeys");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    std::fs::write(env.root.join("config/kraal/runbooks/hello.toml"), "steps = [\"echo hello\"]\n").unwrap();
    env.run(&["open", "w"]);
    let mut client = env.client();
    // ctrl+b R b opens the runbooks, and the rest comes in the same breath.
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\x02Rbecho leaked\r").unwrap();
    std::thread::sleep(Duration::from_millis(800));
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\x1b").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(!screen.contains("leaked"), "typed into the shell: {screen}");
    let _ = client.kill();
    let _ = client.wait();
}

/// A repository's runbook is a file someone else wrote: nothing of it runs before this version
/// of it is approved, and approving is an act of its own. Listing it, showing it, planning it
/// and `--yes` (which answers the plan) approve nothing; `kraal runbook trust` does, for the
/// file as it is.
#[test]
fn a_repository_runbook_runs_only_once_approved() {
    let env = Env::new("rbtrust");
    let repo = env.root.join("app");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let file = repo.join(".kraal/runbooks/deploy.toml");
    let version = |word: &str| {
        format!(
            "description = \"deploy\"\nsteps = [\"echo {word} > {}\"]\n[[param]]\nname = \"V\"\ndefault = \"1\"\nchoices_command = \"touch {}\"\n",
            ran.display(),
            listed.display()
        )
    };
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &version("one"))]);
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let kraal = |args: &[&str]| {
        let mut c = env.cmd(&[&["runbook"], args].concat());
        c.current_dir(&repo).stdin(std::process::Stdio::null()).output().unwrap()
    };
    let text =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    // What a program other than this kraal's command line can do: ask for a plan, then run it.
    let plan = || {
        let request =
            serde_json::json!({ "cmd": "runbook_plan", "path": file, "target": null, "params": [], "pane": null });
        let plan = raw_request(&env, request);
        plan["Ok"]["data"]["token"].as_str().unwrap_or_else(|| panic!("no plan: {plan}")).to_string()
    };
    let run_plan = |token: &str| raw_request(&env, serde_json::json!({ "cmd": "runbook_run", "token": token }));
    let refused = |answer: &serde_json::Value| answer["Err"].as_str().is_some_and(|e| e.contains("is not approved"));

    // Reading it runs nothing and approves nothing.
    for args in [&["list"][..], &["list", "--json"], &["show", "deploy"], &["run", "deploy", "--dry-run"]] {
        let out = kraal(args);
        assert!(out.status.success() && text(&out).contains("deploy"), "{args:?}: {}", text(&out));
        assert!(!ran.exists() && !listed.exists() && !store.exists(), "{args:?}");
    }
    // `--yes` answers the plan, and without a terminal nobody answers: neither approves.
    for args in [&["run", "deploy", "--yes", "--wait"][..], &["run", "deploy"]] {
        let out = kraal(args);
        let said = text(&out);
        assert!(!out.status.success(), "{args:?}: {said}");
        assert!(said.contains("is not approved") && said.contains("`kraal runbook trust deploy`"), "{args:?}: {said}");
        assert!(!ran.exists() && !listed.exists() && !store.exists(), "{args:?}");
    }
    let answer = run_plan(&plan());
    assert!(refused(&answer), "{answer}");
    assert!(!ran.exists() && !store.exists());

    // Approved as it is now: it runs without a question, and so does a plan made before.
    let before = plan();
    let out = kraal(&["trust", "deploy"]);
    assert!(out.status.success() && text(&out).contains("is approved as it is now"), "{}", text(&out));
    let out = kraal(&["run", "deploy", "--yes", "--wait"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "one\n");
    assert!(listed.exists(), "approved, it lists the choices its value is held to");
    std::fs::remove_file(&ran).unwrap();
    let answer = run_plan(&before);
    assert!(answer["Ok"].is_object(), "{answer}");
    wait_until(Duration::from_secs(10), "the plan made before the approval never ran", || {
        std::fs::read_to_string(&ran).is_ok_and(|t| t == "one\n")
    });

    // Changed since: that version was never approved, and lists no more than it runs.
    std::fs::write(&file, version("two")).unwrap();
    std::fs::remove_file(&listed).unwrap();
    let out = kraal(&["run", "deploy", "--yes", "--wait"]);
    assert!(!out.status.success() && text(&out).contains("`kraal runbook trust deploy`"), "{}", text(&out));
    let answer = run_plan(&plan());
    assert!(refused(&answer), "{answer}");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "one\n");
    assert!(!listed.exists());
    let out = kraal(&["trust", "deploy"]);
    assert!(out.status.success(), "{}", text(&out));
    let out = kraal(&["run", "deploy", "--yes", "--wait"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "two\n");

    // What these commands print of a repository's files, a terminal acts on none of it: an
    // escape sequence in a description, in a step, or in a file that cannot be read.
    let odd = "description = \"clear \\u001b[2J\"\nsteps = [\"echo \\u001b[2K\"]\n";
    std::fs::write(repo.join(".kraal/runbooks/odd.toml"), odd).unwrap();
    std::fs::write(repo.join(".kraal/runbooks/bad.toml"), "steps = \x1b[2J\n").unwrap();
    let said = text(&kraal(&["list"]));
    assert!(said.contains("clear \\u{1b}[2J") && said.contains("steps = \\u{1b}[2J"), "{said:?}");
    assert!(!said.contains('\x1b'), "{said:?}");
    let said = text(&kraal(&["run", "odd", "--dry-run"]));
    assert!(said.contains("1. echo \\u{1b}[2K") && !said.contains('\x1b'), "{said:?}");
    assert!(!text(&kraal(&["show", "odd"])).contains('\x1b'));

    // The user's own runbooks need no approval.
    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    std::fs::write(env.root.join("config/kraal/runbooks/mine.toml"), "steps = [\"true\"]\n").unwrap();
    let out = kraal(&["trust", "mine"]);
    assert!(out.status.success() && text(&out).contains("needs no approval"), "{}", text(&out));
    let out = kraal(&["run", "mine", "--yes", "--wait"]);
    assert!(out.status.success(), "{}", text(&out));
}

/// An approval is of a file in its repository, however the way there is spelled: through a link
/// to the checkout or not (`/tmp` is one on macOS), on the command line or in a plan another
/// program asks for, and in a store written by 0.3.0 under the spelling used then. A link that
/// comes to lead to another repository carries no approval there.
#[test]
fn a_runbook_approval_does_not_depend_on_how_its_repository_is_reached() {
    let env = Env::new("rblink");
    let (ran, link) = (env.root.join("ran"), env.root.join("link"));
    let runbook = format!("steps = [\"echo ran >> {}\"]\n", ran.display());
    let repository = |dir: &str| {
        let repo = env.root.join(dir).join("app");
        let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &runbook)]);
        repo
    };
    let (first, second) = (repository("first"), repository("second"));
    std::os::unix::fs::symlink(env.root.join("first"), &link).unwrap();
    // The file where its repository really is, and as a path through the link spells it.
    let file = |repo: &Path| repo.join(".kraal/runbooks/deploy.toml").display().to_string();
    let (real, spelled) = (file(&first.canonicalize().unwrap()), file(&link.join("app")));
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let kraal = |dir: &Path, args: &[&str]| {
        let mut c = env.cmd(&[&["runbook"], args].concat());
        let out = c.current_dir(dir).stdin(std::process::Stdio::null()).output().unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    };
    // A plan of the file at `path`, asked for and run as another program would.
    let run_plan = |path: &str| {
        let request =
            serde_json::json!({ "cmd": "runbook_plan", "path": path, "target": null, "params": [], "pane": null });
        let plan = raw_request(&env, request);
        let token = plan["Ok"]["data"]["token"].as_str().unwrap_or_else(|| panic!("no plan: {plan}")).to_string();
        raw_request(&env, serde_json::json!({ "cmd": "runbook_run", "token": token }))
    };
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let approved = || -> Vec<String> {
        let store: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        store.as_object().unwrap().keys().cloned().collect()
    };

    // Approved in the checkout: approved through the link too, and recorded once.
    let (ok, said) = kraal(&first, &["trust", "deploy"]);
    assert!(ok, "{said}");
    assert_eq!(approved(), std::slice::from_ref(&real));
    let answer = run_plan(&spelled);
    assert!(answer["Ok"].is_object(), "{answer}");
    let (ok, said) = kraal(&link.join("app"), &["run", "deploy", "--yes", "--wait"]);
    assert!(ok, "{said}");

    // A store written by 0.3.0 under the spelling through the link still approves the file; the
    // next approval leaves the file one entry, under its real place.
    let mut written = serde_json::Map::new();
    written.insert(spelled.clone(), serde_json::Value::String(runbook.clone()));
    std::fs::write(&store, serde_json::Value::Object(written).to_string()).unwrap();
    let (ok, said) = kraal(&first, &["run", "deploy", "--yes", "--wait"]);
    assert!(ok, "{said}");
    let answer = run_plan(&real);
    assert!(answer["Ok"].is_object(), "{answer}");
    let (ok, said) = kraal(&first, &["trust", "deploy"]);
    assert!(ok, "{said}");
    assert_eq!(approved(), std::slice::from_ref(&real));

    // The link comes to lead to another repository, with the same file: nothing is approved
    // there.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(second.parent().unwrap(), &link).unwrap();
    let answer = run_plan(&spelled);
    assert!(answer["Err"].as_str().is_some_and(|e| e.contains("is not approved")), "{answer}");
    let (ok, said) = kraal(&link.join("app"), &["run", "deploy", "--yes", "--wait"]);
    assert!(!ok && said.contains("is not approved"), "{said}");
}

/// In a terminal, `kraal runbook run` asks before a repository runbook nobody approved: the
/// question shows the file, with what a terminal would hide written out, names the approval,
/// and comes before the plan's.
#[cfg(target_os = "linux")]
#[test]
fn a_repository_runbook_is_approved_by_a_question_of_its_own() {
    let env = Env::new("rbask");
    let repo = env.root.join("app");
    let ran = env.root.join("ran");
    // A zero-width space, which a terminal shows nothing of.
    let runbook = format!("steps = [\"echo ran > {} #\u{200b}\"]\n", ran.display());
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &runbook)]);
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let run = |input: &[u8]| {
        let mut c = env.cmd(&["runbook", "run", "deploy", "--wait"]);
        c.current_dir(&repo);
        in_terminal(c, input)
    };

    let shown = run(b"n\n");
    assert!(shown.contains("Trust this version?") && shown.contains("#\\u{200b}"), "{shown}");
    assert!(!shown.contains("Run it?"), "{shown}");
    assert!(!ran.exists() && !store.exists());
    // Approved, then its plan declined: nothing runs, and it stays approved.
    let shown = run(b"y\nn\n");
    assert!(shown.contains("Trust this version?") && shown.contains("Run it?"), "{shown}");
    assert!(!ran.exists() && store.exists());
    let shown = run(b"y\n");
    assert!(!shown.contains("Trust this version?") && shown.contains("finished: 1 ok"), "{shown}");
    assert!(ran.exists());
}

/// From the list of runbooks, a repository runbook asks to be trusted before anything of it
/// runs, its `choices_command` included; a no leaves it as it was, and a change asks again. A
/// Markdown one asks before its first block is typed.
#[test]
fn the_runbook_list_asks_before_a_repository_runbook_does_anything() {
    let env = Env::new("rbpick");
    let repo = env.root.join("app");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let file = repo.join(".kraal/runbooks/deploy.toml");
    let version = |word: &str| {
        format!(
            "steps = [\"echo {word} $V > {}\"]\n[[param]]\nname = \"V\"\nchoices_command = \"touch {}; echo first\"\n",
            ran.display(),
            listed.display()
        )
    };
    let _ = git_repo(
        &repo,
        &[
            (".kraal/runbooks/deploy.toml", &version("one")),
            (".kraal/runbooks/notes.md", "# Notes\n\n```sh\necho from-\"notes\"\n```\n"),
        ],
    );
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    env.run(&["open", "w"]);
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let mut client = RawClient::attach(&env, "w");
    // The list has the repository's runbooks once git has seen it.
    client.wait_for_screen("⎇ main");
    let pick = |client: &mut RawClient, name: &str| {
        env.run(&["focus", "shell"]);
        client.send(b"\x02Rb");
        client.wait_for_row("Runbooks");
        client.send(name.as_bytes());
        client.wait_for_row("repository app");
        client.send(b"\r");
    };
    let gone = |client: &RawClient, text: &str| {
        client.wait_for_rows(&format!("without {text:?}"), |rows| !rows.iter().any(|r| r.contains(text)));
    };

    pick(&mut client, "deploy");
    client.wait_for_row("Trust the runbook deploy");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listed.exists() && !ran.exists() && !store.exists(), "something ran before the answer");
    client.send(b"n");
    gone(&client, "Trust the runbook deploy");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listed.exists() && !ran.exists() && !store.exists(), "a no approved or ran something");

    // Approved: its choices are listed, then its plan is shown.
    pick(&mut client, "deploy");
    client.wait_for_row("Trust the runbook deploy");
    client.send(b"y");
    gone(&client, "Trust the runbook deploy");
    client.wait_for_row("first");
    assert!(listed.exists() && !ran.exists());
    client.send(b"\r");
    client.wait_for_row("Run deploy on this machine?");
    client.send(b"y");
    wait_until(Duration::from_secs(10), "the approved runbook never ran", || {
        std::fs::read_to_string(&ran).is_ok_and(|t| t == "one first\n")
    });

    // Changed since: asked again, before its choices are listed.
    std::fs::remove_file(&listed).unwrap();
    std::fs::write(&file, version("two")).unwrap();
    pick(&mut client, "deploy");
    client.wait_for_row("Trust the runbook deploy");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listed.exists());
    client.send(b"n");
    gone(&client, "Trust the runbook deploy");

    // A document: its first block waits for the answer; approved from the command line, the
    // block is typed without a question.
    pick(&mut client, "notes");
    client.wait_for_row("1. Notes");
    client.send(b"\r");
    client.wait_for_row("From the repository");
    client.send(b"n");
    gone(&client, "From the repository");
    assert!(!env.run(&["read", "-p", "shell"]).contains("from-notes"));
    let out = env.cmd(&["runbook", "trust", "notes"]).current_dir(&repo).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    env.run(&["focus", "shell"]);
    client.send(b"\x02Rb");
    client.wait_for_row("1. Notes");
    client.send(b"\r");
    env.wait_for_text("shell", "from-notes");
}

/// The question that asks to trust a repository's runbook shows all that a yes lets run,
/// whatever its length and the terminal's size: every line of every step with its spacing, a
/// long line whole, what a terminal would hide written out, the shell, and the commands that
/// list choices. Reading it runs and approves nothing, and Enter cancels. A document's shows the
/// step in full, then its other steps.
#[test]
fn all_that_a_repository_runbook_runs_can_be_read_before_it_is_trusted() {
    let env = Env::new("rbread");
    let repo = env.root.join("app");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let steps: Vec<String> = (1..=40)
        .map(|i| match i {
            7 => "echo safe \u{1b}[2K\u{200b}".to_string(),
            40 => format!("echo {} > {}", "0123456789".repeat(20), ran.display()),
            _ => format!("if test -n \"$V\"; then\n    echo  step {i}\n\nfi"),
        })
        .collect();
    let choices = format!("touch {}\n  echo first", listed.display());
    let quoted: Vec<String> = steps.iter().map(|step| serde_json::json!(step).to_string()).collect();
    let deploy = format!(
        "shell = \"bash\"\nsteps = [{}]\n[[param]]\nname = \"V\"\nchoices_command = {}\n",
        quoted.join(", "),
        serde_json::json!(choices)
    );
    let build = "for f in a b; do\n    echo  from-\"notes\" \"$f\"\n\ndone\n".to_string() + &"echo more\n".repeat(8);
    let notes = format!(
        "# Notes\n\n## Build\n\n```sh\n{build}```\n\n## Deploy\n\n```sh\necho one\n  echo two\n```\n\n\
         ## Check\n\n```console\n$ echo three\nthree\n```\n"
    );
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &deploy), (".kraal/runbooks/notes.md", &notes)]);
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    env.run(&["open", "w"]);
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let pick = |client: &mut RawClient, name: &str| {
        env.run(&["focus", "shell"]);
        client.send(b"\x02Rb");
        client.wait_for_row("Runbooks");
        client.send(name.as_bytes());
        client.wait_for_row("repository app");
        client.send(b"\r");
    };
    let gone = |client: &RawClient, text: &str| {
        client.wait_for_rows(&format!("without {text:?}"), |rows| !rows.iter().any(|r| r.contains(text)));
    };
    // A step as the question quotes it: under its number, each line where it is in the file.
    let mut expected: Vec<String> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        expected.extend(numbered(i + 1, &step.replace('\u{1b}', "\\u{1b}").replace('\u{200b}', "\\u{200b}")));
    }
    expected.push("To list the choices of V, it runs:".to_string());
    expected.extend(choices.lines().map(|line| format!("     {line}")));
    let expected = without_trailing_spaces(expected);
    // The sidebar is there from 84 columns on; a page key moves by the rows the box has.
    for (size, key) in [((124, 36), &b"\x1b[6~"[..]), ((80, 24), b" "), ((60, 12), b"\x1b[6~")] {
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        if size.0 == 124 {
            // The list has the repository's runbooks once git has seen it.
            client.wait_for_screen("⎇ main");
        }
        pick(&mut client, "deploy");
        client.wait_for_row("Trust the runbook deploy");
        let lines = client.read_question(key);
        let first = lines.iter().position(|line| line.starts_with("  1. ")).unwrap();
        assert_eq!(lines[first..], expected[..], "at {size:?}");
        let said = lines[..first].join(" ");
        assert!(said.contains("It runs these steps with bash:"), "at {size:?}: {said}");
        assert!(!listed.exists() && !ran.exists() && !store.exists(), "reading it ran or approved something");
        // Cancel is the button selected.
        client.send(b"\r");
        gone(&client, "Trust the runbook deploy");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!listed.exists() && !store.exists(), "Enter approved it");
    }

    // A document: the step that would be typed, whole, then the others a yes approves with it.
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    pick(&mut client, "notes");
    client.wait_for_row("1. Build");
    client.send(b"\r");
    client.wait_for_row("Run step 1 in");
    let lines = client.read_question(b"j");
    let mut expected: Vec<String> = build.lines().map(|line| format!("  {line}")).collect();
    expected.push("Its other steps:".to_string());
    expected.extend(numbered(2, "echo one\n  echo two"));
    expected.extend(numbered(3, "echo three"));
    let first = lines.iter().position(|line| line.starts_with("  for f in")).unwrap();
    assert_eq!(lines[first..], without_trailing_spaces(expected)[..]);
    let said = lines[..first].join(" ");
    assert!(said.contains("A yes approves it as it is now, with all its steps."), "{said}");
    client.send(b"\r");
    gone(&client, "Run step 1 in");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!env.run(&["read", "-p", "shell"]).contains("from-notes") && !store.exists());
}
