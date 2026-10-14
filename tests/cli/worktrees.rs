use super::support::RawClient;
#[cfg(target_os = "linux")]
use super::support::{in_terminal, numbered, without_trailing_spaces};
#[cfg(target_os = "linux")]
use crate::common::wait_until;
use crate::common::{Env, git_repo, write_script};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

#[test]
fn worktrees_for_agents() {
    let env = Env::new("worktrees");
    let repo = env.root.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-q", "-m", "init"]);
    let wt = |args: &[&str]| env.cmd(&[&["wt"], args].concat()).current_dir(&repo).output().unwrap();
    let out = wt(&["new", "fix-login"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let path = env.root.join("app.worktrees/fix-login");
    assert!(path.join("README.md").exists());
    let list = String::from_utf8(wt(&["list"]).stdout).unwrap();
    assert!(list.contains("fix-login") && list.contains("kraal/fix-login"), "{list}");
    std::fs::write(path.join("README.md"), "changed\n").unwrap();
    assert!(!wt(&["rm", "fix-login"]).status.success(), "uncommitted changes need --force");
    assert!(wt(&["rm", "fix-login", "--force"]).status.success());
    assert!(!path.exists());
}

/// What git says of a pane's directory follows what is done in its repository from elsewhere,
/// promptly, while the pane prints nothing: a checkout, a commit.
#[test]
fn git_in_frames_follows_changes_made_elsewhere() {
    let env = Env::new("gitwatch");
    let repo = env.root.join("repo");
    let git = git_repo(&repo, &[("a.txt", "1")]);
    // An upstream, for the commits ahead of it.
    git(&["config", "remote.origin.url", "/nowhere"]);
    git(&["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);
    git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    env.workspace("w", &format!("layout {{\n    pane name=\"a\" cwd={:?}\n}}\n", repo.display().to_string()));
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    client.wait_for_screen("main");
    env.wait_for_text("a", "$");
    // Long enough for the server to be idle.
    std::thread::sleep(Duration::from_secs(4));
    let start = Instant::now();
    git(&["checkout", "-q", "-b", "qqbranch", "--track", "origin/main"]);
    client.wait_for_screen("qqbranch");
    assert!(start.elapsed() < Duration::from_secs(2), "a checkout showed after {:?}", start.elapsed());
    // A commit replaces the branch's ref, not the index.
    std::thread::sleep(Duration::from_secs(2));
    let start = Instant::now();
    git(&["commit", "-q", "--allow-empty", "-m", "two"]);
    client.wait_for_screen("↑1");
    assert!(start.elapsed() < Duration::from_secs(2), "a commit showed after {:?}", start.elapsed());
}

#[test]
fn worktrees_are_prepared_numbered_and_pruned_from_the_command_line() {
    let env = Env::new("wtcli");
    let repo = env.root.join("app");
    let git = git_repo(
        &repo,
        &[
            (".gitignore", ".env*\nsetup.log\n"),
            (
                ".kraal/worktree.toml",
                "copy = [\".env*\"]\nsetup = ['echo \"$KRAAL_WORKTREE #$KRAAL_WORKTREE_INDEX on $PORT\" > setup.log']\n",
            ),
        ],
    );
    std::fs::write(repo.join(".env"), "A=1\n").unwrap();
    std::fs::write(env.root.join("secret"), "private\n").unwrap();
    std::os::unix::fs::symlink(env.root.join("secret"), repo.join(".env.prod")).unwrap();
    let config = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[worktrees]\nport_base = 3000\n");
    std::fs::write(&config, text).unwrap();
    let wt = |args: &[&str]| env.cmd(&[&["wt"], args].concat()).current_dir(&repo).output().unwrap();
    let text =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let dir = env.root.join("app.worktrees");

    // Not approved yet, and nobody to ask: nothing is made.
    let out = wt(&["new", "one"]);
    assert!(!out.status.success() && text(&out).contains("`kraal wt trust`"), "{}", text(&out));
    assert!(!dir.join("one").exists());
    // Left as git makes it.
    assert!(wt(&["new", "one", "--no-setup"]).status.success());
    assert!(!dir.join("one/.env").exists());
    // Approved: the files are copied, never through a link out of the repository, and the
    // commands run with the worktree's number and port.
    assert!(wt(&["trust"]).status.success());
    let out = wt(&["new", "two", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(dir.join("two/setup.log")).unwrap(), "two #2 on 3020\n");
    assert_eq!(std::fs::read_to_string(dir.join("two/.env")).unwrap(), "A=1\n");
    assert!(!dir.join("two/.env.prod").exists());
    assert!(text(&out).contains("not copied: .env.prod (a link out of the repository)"), "{}", text(&out));
    // That version runs again without asking.
    let out = wt(&["new", "three"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(dir.join("three/setup.log")).unwrap(), "three #3 on 3030\n");
    // Changed: asked again. A failing command fails the command; the worktree stays.
    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"exit 3\"]\n").unwrap();
    git(&["commit", "-q", "-am", "fail"]);
    assert!(!wt(&["new", "four"]).status.success());
    assert!(!dir.join("four").exists());
    assert!(wt(&["trust"]).status.success());
    let out = wt(&["new", "four", "--yes"]);
    assert!(!out.status.success() && text(&out).contains("the setup of four failed (exit 3)"), "{}", text(&out));
    assert!(dir.join("four").exists());
    let list = String::from_utf8(wt(&["list"]).stdout).unwrap();
    assert!(list.lines().any(|l| l.starts_with("two") && l.contains("#2")), "{list}");

    // Merged and clean ones go; one with a change or a commit of its own stays, and says why.
    std::fs::write(dir.join("three/notes.txt"), "draft\n").unwrap();
    let in_two = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir.join("two"))
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::write(dir.join("two/feature.txt"), "x\n").unwrap();
    in_two(&["add", "feature.txt"]);
    in_two(&["commit", "-q", "-m", "feature"]);
    let out = wt(&["prune", "--dry-run"]);
    let said = text(&out);
    assert!(said.contains("would remove") && said.contains("kraal/one") && said.contains("kraal/four"), "{said}");
    assert!(said.contains("kept three: it has 1 uncommitted change"), "{said}");
    assert!(said.contains("kept two: it has 1 unmerged commit"), "{said}");
    assert!(dir.join("one").exists(), "a dry run removes nothing");
    let out = wt(&["prune"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.join("one").exists() && !dir.join("four").exists());
    assert!(dir.join("two/feature.txt").exists() && dir.join("three/notes.txt").exists());
    let list = String::from_utf8(wt(&["list"]).stdout).unwrap();
    assert!(!list.contains("one") && list.contains("two") && list.contains("three"), "{list}");
}

/// A repository's `.kraal/worktree.toml` is a file someone else wrote: kraal copies nothing it
/// names and runs none of its commands before this version of it is approved, and approving is
/// an act of its own (`kraal wt trust`). `--yes` asks nothing and approves nothing, with or
/// without an agent to start; `kraal wt setup` shows the file and does nothing else.
#[test]
fn a_worktree_setup_is_acted_on_only_once_approved() {
    let env = Env::new("wttrust");
    let repo = env.root.join("app");
    let ran = env.root.join("ran");
    let file = repo.join(".kraal/worktree.toml");
    let setup = |word: &str| format!("copy = [\".env\"] # \u{200b}\nsetup = [\"echo {word} >> {}\"]\n", ran.display());
    let first = setup("one");
    let committed = [(".gitignore", ".env\nnotes\n"), (".kraal/worktree.toml", first.as_str())];
    let git = git_repo(&repo, &committed);
    std::fs::write(repo.join(".env"), "A=1\n").unwrap();
    let wt_in = |repo: &Path, args: &[&str]| {
        let mut c = env.cmd(&[&["wt"], args].concat());
        let out = c.current_dir(repo).stdin(std::process::Stdio::null()).output().unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    };
    let wt = |args: &[&str]| wt_in(&repo, args);
    let dir = env.root.join("app.worktrees");
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();

    // Shown as a new worktree would get it, with what a terminal would hide written out.
    let (ok, shown) = wt(&["setup"]);
    assert!(ok && shown.contains(".kraal/worktree.toml as of main\n"), "{shown}");
    assert!(shown.contains("# \\u{200b}\n") && shown.contains("echo one >> "), "{shown}");
    assert!(!store.exists() && !dir.exists() && !ran.exists());

    // `--yes` asks nothing, and nobody answers without a terminal: neither approves.
    let refused: [&[&str]; 3] = [&["new", "x", "--yes"], &["new", "x"], &["new", "x", "--agent", "claude", "--yes"]];
    for args in refused {
        let (ok, said) = wt(args);
        assert!(!ok && said.contains("the worktree setup comes from the repository "), "{args:?}: {said}");
        assert!(said.contains("is not approved: read it (`kraal wt setup`)"), "{args:?}: {said}");
        assert!(said.contains("approve it (`kraal wt trust`)") && said.contains("--no-setup"), "{args:?}: {said}");
        assert!(!store.exists() && !dir.exists() && !ran.exists(), "{args:?}");
    }
    // Without the setup, the worktree is made, and nothing of the file is acted on.
    let (ok, said) = wt(&["new", "bare", "--no-setup", "--yes"]);
    assert!(ok && dir.join("bare").exists() && !dir.join("bare/.env").exists() && !ran.exists(), "{said}");

    // Approved as it is now: its files are copied and its commands run, without a question.
    let (ok, said) = wt(&["trust"]);
    assert!(ok && said.contains("is approved as it is now"), "{said}");
    let (ok, said) = wt(&["new", "x", "--yes"]);
    assert!(ok, "{said}");
    assert_eq!((read(&ran).as_str(), read(&dir.join("x/.env")).as_str()), ("one\n", "A=1\n"));

    // Changed since, be it only in what it copies: that version was never approved.
    std::fs::write(&file, "copy = [\".env\", \"notes\"]\n").unwrap();
    std::fs::write(repo.join("notes"), "private\n").unwrap();
    git(&["commit", "-q", "-am", "copy more"]);
    let (ok, said) = wt(&["new", "y", "--yes"]);
    assert!(!ok && said.contains("`kraal wt trust`") && !dir.join("y").exists(), "{said}");
    assert!(wt(&["trust"]).0);
    let (ok, said) = wt(&["new", "y", "--yes"]);
    assert!(ok && read(&dir.join("y/notes")) == "private\n", "{said}");

    // The file of another branch is another version: shown and approved by that branch's name.
    git(&["checkout", "-q", "-b", "other"]);
    std::fs::write(&file, setup("two")).unwrap();
    git(&["commit", "-q", "-am", "two"]);
    git(&["checkout", "-q", "main"]);
    let (ok, said) = wt(&["new", "z", "--base", "other", "--yes"]);
    assert!(!ok && said.contains("(`kraal wt setup --base other`)"), "{said}");
    assert!(said.contains("(`kraal wt trust --base other`)") && !dir.join("z").exists(), "{said}");
    let (ok, shown) = wt(&["setup", "--base", "other"]);
    assert!(ok && shown.contains("worktree.toml as of other\n") && shown.contains("echo two >> "), "{shown}");
    assert!(wt(&["trust", "--base", "other"]).0);
    let (ok, said) = wt(&["new", "z", "--base", "other", "--yes"]);
    assert!(ok && read(&ran) == "one\ntwo\n", "{said}");

    // A repository without the file has nothing to approve.
    let plain = env.root.join("plain");
    let _ = git_repo(&plain, &[("a.txt", "1\n")]);
    let (ok, said) = wt_in(&plain, &["setup"]);
    assert!(ok && said.starts_with("no ") && said.contains(".kraal/worktree.toml as of main"), "{said}");
    let (ok, said) = wt_in(&plain, &["trust"]);
    assert!(ok && said.contains("nothing to approve"), "{said}");
    let (ok, said) = wt_in(&plain, &["new", "n", "--yes"]);
    assert!(ok && env.root.join("plain.worktrees/n").exists(), "{said}");
}

/// In a terminal, `kraal wt new` shows the repository's setup, every command of it, and asks
/// whether to trust it before anything is made; `--yes` there asks nothing and approves
/// nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_worktree_setup_is_approved_in_a_terminal_by_a_question_that_says_so() {
    let env = Env::new("wtask");
    let repo = env.root.join("app");
    let ran = env.root.join("ran");
    let setup = format!("setup = [\"echo ran >> {}\"]\n", ran.display());
    let _ = git_repo(&repo, &[(".kraal/worktree.toml", &setup)]);
    let dir = env.root.join("app.worktrees");
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let new = |args: &[&str], typed: &[u8]| {
        let mut c = env.cmd(&[&["wt", "new"], args].concat());
        c.current_dir(&repo);
        in_terminal(c, typed)
    };

    let shown = new(&["x"], b"n\n");
    assert!(
        shown.contains("Trust the worktree setup of the repository ") && shown.contains("  1. echo ran >> "),
        "{shown}"
    );
    assert!(shown.contains("Trust this version and prepare the worktree with it? [y/N]"), "{shown}");
    assert!(shown.contains("cancelled; nothing was created"), "{shown}");
    assert!(!dir.exists() && !ran.exists() && !store.exists());
    let shown = new(&["x", "--yes"], b"y\n");
    assert!(shown.contains("is not approved") && !shown.contains("[y/N]"), "{shown}");
    assert!(!dir.exists() && !ran.exists() && !store.exists());
    let shown = new(&["x"], b"y\n");
    assert!(shown.contains("[y/N]") && dir.join("x").exists() && ran.exists() && store.exists(), "{shown}");
    // Approved: the next one is made without a question.
    let shown = new(&["y"], b"");
    assert!(!shown.contains("Trust") && dir.join("y").exists(), "{shown}");
}

/// A worktree's setup runs once, when the worktree is made. A session restarted while it runs
/// brings the agent's pane back and runs nothing of the repository's again.
#[cfg(target_os = "linux")]
#[test]
fn a_worktree_setup_is_not_run_again_after_a_restart() {
    let env = Env::new("wtrestart");
    let repo = env.root.join("app");
    let ran = env.root.join("ran");
    let setup = format!("setup = [\"echo started >> {}\", \"sleep 30\"]\n", ran.display());
    let _ = git_repo(&repo, &[(".kraal/worktree.toml", &setup)]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\necho agent started\nexec sleep 100\n");
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"beta\"]\n\n[[account]]\nname=\"beta\"\nagent=\"claude\"\nhome={:?}\n",
            home.display().to_string(), env.root.join("beta-home").display().to_string()
        ),
    )
    .unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let open = || {
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    open();
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("⎇ main");
    client.send(b"\x02a");
    client.wait_for_row("Claude · alpha");
    client.send(b"\x17");
    client.wait_for_screen("Name of the worktree");
    client.send(b"feat\r");
    client.wait_for_screen("Trust the worktree setup");
    client.send(b"y");
    let started = || std::fs::read_to_string(&ran).unwrap_or_default();
    wait_until(Duration::from_secs(15), "the setup never started", || started() == "started\n");

    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let agent = panes.as_array().unwrap().iter().find(|p| p["name"] == "claude feat").unwrap();
    assert_eq!(agent["account"], "claude:alpha", "the selected account overrides the beta rule");

    // Stopped while the setup runs, and started again: the pane is back, the setup is not.
    env.run(&["kill"]);
    open();
    wait_until(Duration::from_secs(10), "the agent's pane did not come back", || {
        env.run(&["panes"]).contains("claude feat")
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(started(), "started\n", "the setup ran again");
}

/// From the agent picker: the worktree is made once its repository's setup is approved, the
/// setup runs in the agent's pane, and the agent starts there with the worktree's number. A
/// setup that fails leaves the agent waiting for Enter; a no makes nothing. Merged worktrees
/// are then removed in one go, their panes closed.
#[cfg(target_os = "linux")]
#[test]
fn an_agent_worktree_is_prepared_in_its_pane_once_trusted() {
    let env = Env::new("wtsetup");
    let repo = env.root.join("app");
    let git = git_repo(
        &repo,
        &[
            (".gitignore", ".env\n"),
            (
                ".kraal/worktree.toml",
                "copy = [\".env\"]\nsetup = ['echo \"preparing $KRAAL_WORKTREE\"', \"test -f .env\"]\n",
            ),
        ],
    );
    std::fs::write(repo.join(".env"), "A=1\n").unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\necho \"agent in $KRAAL_WORKTREE #$KRAAL_WORKTREE_INDEX port $PORT\"\nexec sleep 100\n",
    );
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"alpha\"]\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    let config = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[worktrees]\nport_base = 3000\n");
    std::fs::write(&config, text).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach(&env, "w");
    // The agent picker offers worktrees once git has seen the repository.
    client.wait_for_screen("⎇ main");
    let new_worktree = |client: &mut RawClient, name: &str| {
        env.run(&["focus", "shell"]);
        client.screen.lock().unwrap().clear();
        client.send(b"\x02a");
        client.wait_for_row("Claude · alpha");
        client.send(b"\x17");
        client.wait_for_screen("Name of the worktree");
        client.send(name.as_bytes());
        client.send(b"\r");
        client.wait_for_screen("Trust the worktree setup");
    };
    let dir = env.root.join("app.worktrees");
    // The pane comes once git has made the worktree.
    let wait_for = |pane: &str, text: &str| -> String {
        let start = Instant::now();
        loop {
            let out = env.try_run(&["read", "-p", pane]);
            let screen = String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success() && screen.contains(text) {
                return screen;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "timed out waiting for {text:?} in {pane}:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    new_worktree(&mut client, "feat");
    assert!(!dir.join("feat").exists(), "nothing is made before the answer");
    client.send(b"y");
    let screen = wait_for("claude feat", "agent in feat #1 port 3010");
    assert!(screen.contains("preparing feat") && screen.contains("copied from the main checkout: .env"), "{screen}");

    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"echo about to fail\", \"exit 3\"]\n").unwrap();
    git(&["commit", "-q", "-am", "fail"]);
    new_worktree(&mut client, "bad");
    client.send(b"y");
    let screen = wait_for("claude bad", "Enter starts this anyway");
    assert!(screen.contains("about to fail") && !screen.contains("agent in"), "{screen}");
    env.run(&["focus", "claude bad"]);
    client.send(b"\r");
    wait_for("claude bad", "agent in bad #2 port 3020");

    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"echo changed\"]\n").unwrap();
    git(&["commit", "-q", "-am", "changed"]);
    new_worktree(&mut client, "no");
    client.send(b"n");
    client.wait_for_row("the worktree no was not created");
    assert!(!dir.join("no").exists());

    // Both worktrees have nothing to lose: they go, with their agents' panes.
    env.run(&["focus", "shell"]);
    client.screen.lock().unwrap().clear();
    client.send(b"\x02Rg");
    client.wait_for_screen("Remove merged worktrees");
    client.send(b"\r");
    client.wait_for_screen("Closes these panes");
    client.send(b"y");
    let start = Instant::now();
    while dir.join("feat").exists() || dir.join("bad").exists() {
        assert!(start.elapsed() < Duration::from_secs(15), "the merged worktrees are still there");
        std::thread::sleep(Duration::from_millis(100));
    }
    let panes = env.run(&["panes"]);
    assert!(!panes.contains("claude feat") && !panes.contains("claude bad"), "{panes}");

    // From the command line inside the session: the version declined above is refused there
    // too, `--yes` or not; approved, the worktree is prepared there, and the agent opened in the
    // session gets the worktree's number (the first, free again).
    let socket = env.root.join("run/kraal/wtsetup.sock");
    let new_cli = || {
        env.cmd(&["wt", "new", "cli", "--agent", "claude", "--yes"])
            .env("PATH", &path)
            .env("KRAAL_SOCKET", &socket)
            .current_dir(&repo)
            .output()
            .unwrap()
    };
    let out = new_cli();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("`kraal wt trust`"));
    assert!(!dir.join("cli").exists());
    let trust = || {
        let out = env.cmd(&["wt", "trust"]).current_dir(&repo).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    trust();
    let out = new_cli();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8(out.stdout).unwrap();
    assert!(said.contains("changed"), "the setup ran there");
    // Its last line is the id of the agent's pane.
    let pane = said.lines().last().unwrap();
    assert!(pane.parse::<u32>().is_ok(), "{said}");
    wait_for(pane, "agent in cli #1 port 3010");

    // Files to copy alone are asked about as well: the repository names them.
    std::fs::write(repo.join(".kraal/worktree.toml"), "copy = [\".env\"]\n").unwrap();
    git(&["commit", "-q", "-am", "copy only"]);
    new_worktree(&mut client, "copy");
    client.wait_for_screen("it copies from the main checkout: .env");
    client.send(b"n");
    client.wait_for_row("the worktree copy was not created");
    assert!(!dir.join("copy").exists());
    // Approved from the command line: the picker asks nothing more, and the file is copied.
    trust();
    env.run(&["focus", "shell"]);
    client.screen.lock().unwrap().clear();
    client.send(b"\x02a");
    client.wait_for_row("Claude · alpha");
    client.send(b"\x17");
    client.wait_for_screen("Name of the worktree");
    client.send(b"copied\r");
    wait_for("claude copied", "agent in copied #2 port 3020");
    assert_eq!(std::fs::read_to_string(dir.join("copied/.env")).unwrap(), "A=1\n");
}

/// The question that asks to trust a repository's worktree setup shows all of it, however long:
/// what it copies, and every line of each command with its spacing. Reading it makes nothing,
/// and Enter cancels.
#[cfg(target_os = "linux")]
#[test]
fn all_of_a_worktree_setup_can_be_read_before_it_is_trusted() {
    let env = Env::new("wtread");
    let repo = env.root.join("app");
    let commands: Vec<String> =
        (1..=12).map(|i| format!("if test -d src; then\n    echo  prepare {i}\n\nfi")).collect();
    let quoted: Vec<String> = commands.iter().map(|command| serde_json::json!(command).to_string()).collect();
    let setup = format!("copy = [\".env\", \"config/*.local\"]\nsetup = [{}]\n", quoted.join(", "));
    let _ = git_repo(&repo, &[(".kraal/worktree.toml", &setup)]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\nexec sleep 100\n");
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"alpha\"]\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let commands = commands.iter().enumerate().flat_map(|(i, command)| numbered(i + 1, command));
    let expected = without_trailing_spaces(commands.collect());

    for (size, key) in [((124, 36), &b"\x1b[6~"[..]), ((80, 24), b"\x1b[B")] {
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        if size.0 == 124 {
            // The agent picker offers worktrees once git has seen the repository.
            client.wait_for_screen("⎇ main");
        }
        env.run(&["focus", "shell"]);
        client.send(b"\x02a");
        client.wait_for_row("Claude · alpha");
        client.send(b"\x17");
        client.wait_for_screen("Name of the worktree");
        client.send(b"feat\r");
        client.wait_for_row("Trust the worktree setup");
        let lines = client.read_question(key);
        let runs = lines.iter().position(|line| line == "it runs there:").unwrap();
        assert_eq!(lines[runs - 1], "it copies from the main checkout: .env, config/*.local", "at {size:?}");
        assert_eq!(lines[runs + 1..], expected[..], "at {size:?}");
        // Cancel is the button selected.
        client.send(b"\r");
        client.wait_for_row("the worktree feat was not created");
        assert!(!env.root.join("app.worktrees").exists(), "reading the question made something");
        assert!(!env.root.join("state/kraal/trusted-runbooks.json").exists(), "Enter approved it");
    }
}

/// Opening the worktree actions of a shell that went into a worktree never waits for git on the
/// server's loop: what git said about the pane's directory is enough.
#[cfg(target_os = "linux")]
#[test]
fn worktree_actions_never_wait_for_git() {
    let env = Env::new("wtslow");
    let repo = env.root.join("app");
    let _git = git_repo(&repo, &[("a.txt", "1\n")]);
    let out = env.cmd(&["wt", "new", "x", "--no-setup"]).current_dir(&repo).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let wt = env.root.join("app.worktrees/x");
    // A git slow to tell a worktree's base, once `slow` exists.
    let real = Command::new("sh").arg("-c").arg("command -v git").output().unwrap();
    let real = String::from_utf8(real.stdout).unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let slow = env.root.join("slow");
    write_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in *kraalbase*) if [ -e '{}' ]; then sleep 5; fi ;; esac\nexec '{}' \"$@\"\n",
            slow.display(),
            real.trim()
        ),
    );
    env.workspace("w", &format!("layout {{\n    pane name=\"sh\" cwd={:?}\n}}\n", wt.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("kraal/x");
    std::fs::write(&slow, "").unwrap();
    client.send(b"\x02Rg");
    std::thread::sleep(Duration::from_millis(150));
    let start = Instant::now();
    env.run(&["panes"]);
    let waited = start.elapsed();
    std::fs::remove_file(&slow).unwrap();
    assert!(waited < Duration::from_millis(1000), "the server waited {waited:?} for git");
    client.wait_for_screen("Worktree x");
}

/// A pane opened from the command line in a worktree the session has not seen yet gets its
/// variables (number, port offset, port).
#[test]
fn a_pane_opened_in_an_unseen_worktree_gets_its_variables() {
    let env = Env::new("wtvars");
    let repo = env.root.join("app");
    let _git = git_repo(&repo, &[("a.txt", "1\n")]);
    let config = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[worktrees]\nport_base = 3000\n");
    std::fs::write(&config, text).unwrap();
    for name in ["x", "y"] {
        let out = env.cmd(&["wt", "new", name, "--no-setup"]).current_dir(&repo).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let wt = env.root.join("app.worktrees/x");
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    let probe = "echo \"idx=$KRAAL_WORKTREE_INDEX offset=$KRAAL_PORT_OFFSET port=$PORT\"; exec sleep 60";
    env.run(&["run", "--name", "probe", "--cwd", &wt.display().to_string(), "--", "sh", "-c", probe]);
    env.wait_for_text("probe", "idx=1 offset=10 port=3010");
    // Reached through a symbolic link (as /var is /private/var on macOS), while git gives the
    // worktree's path with its links resolved.
    std::os::unix::fs::symlink(&env.root, env.root.join("link")).unwrap();
    let through = env.root.join("link/app.worktrees/y");
    env.run(&["run", "--name", "linked", "--cwd", &through.display().to_string(), "--", "sh", "-c", probe]);
    env.wait_for_text("linked", "idx=2 offset=20 port=3020");
}

/// The worktrees of a repository are made and removed one at a time, whoever asks at the same
/// moment (several `kraal wt` here, as a script, or the picker beside it, would): git does not
/// keep two `git worktree add` apart, and one that reads the entry another is writing fails.
/// Each worktree still gets a number of its own.
#[test]
fn the_worktrees_of_a_repository_are_changed_one_at_a_time() {
    let env = Env::new("wtserial");
    let repo = env.root.join("app");
    let _git = git_repo(&repo, &[("a.txt", "1\n")]);
    // A git that notes when two change the worktrees at the same moment, and takes long enough
    // over it for the others to come meanwhile.
    let real = Command::new("sh").arg("-c").arg("command -v git").output().unwrap();
    let real = String::from_utf8(real.stdout).unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let (busy, overlaps) = (env.root.join("busy"), env.root.join("overlaps"));
    write_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\ncase \" $* \" in\n*' worktree add '*|*' worktree remove '*)\n  \
             mkdir '{busy}' 2>/dev/null || echo \"$*\" >> '{overlaps}'\n  sleep 0.1\n  '{real}' \"$@\"\n  \
             status=$?\n  rmdir '{busy}' 2>/dev/null\n  exit $status ;;\nesac\nexec '{real}' \"$@\"\n",
            busy = busy.display(),
            overlaps = overlaps.display(),
            real = real.trim()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let names = ["a", "b", "c", "d", "e"];
    let all_at_once = |verb: &str, flag: &str| {
        std::thread::scope(|scope| {
            let (env, path, repo) = (&env, &path, &repo);
            let runs: Vec<_> = names
                .iter()
                .map(|name| {
                    scope.spawn(move || {
                        env.cmd(&["wt", verb, name, flag]).env("PATH", path).current_dir(repo).output().unwrap()
                    })
                })
                .collect();
            for run in runs {
                let out = run.join().unwrap();
                assert!(out.status.success(), "wt {verb}: {}", String::from_utf8_lossy(&out.stderr));
            }
        });
        let overlaps = std::fs::read_to_string(&overlaps).unwrap_or_default();
        assert_eq!(overlaps, "", "wt {verb}: two at the same moment");
    };
    all_at_once("new", "--no-setup");
    let number = |name: &&str| {
        let recorded = std::fs::read_to_string(repo.join(".git/worktrees").join(name).join("kraal-index")).unwrap();
        recorded.trim().parse::<u32>().unwrap()
    };
    let mut numbers: Vec<u32> = names.iter().map(number).collect();
    numbers.sort();
    assert_eq!(numbers, [1, 2, 3, 4, 5]);
    all_at_once("rm", "--force");
    let left = std::fs::read_dir(repo.join(".git/worktrees")).map_or(0, |entries| entries.count());
    assert_eq!(left, 0, "worktrees left");
}

/// `kraal wt prune` removes a branch merged by squash (its commits are not in the base's
/// history, their changes are), keeps one whose ignored files are its own unless told to discard
/// them, and `--no-setup` makes a worktree even when the repository's setup file is broken.
#[test]
fn merged_worktrees_are_pruned_however_they_were_merged() {
    let env = Env::new("wtsquash");
    let repo = env.root.join("app");
    let git = git_repo(&repo, &[(".gitignore", "*.db\n"), ("a.txt", "1\n")]);
    let wt = |args: &[&str]| env.cmd(&[&["wt"], args].concat()).current_dir(&repo).output().unwrap();
    let text =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let dir = env.root.join("app.worktrees");
    for name in ["sq", "data"] {
        assert!(wt(&["new", name, "--no-setup"]).status.success());
    }
    let in_sq = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir.join("sq"))
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::write(dir.join("sq/a.txt"), "2\n").unwrap();
    in_sq(&["commit", "-q", "-am", "two"]);
    std::fs::write(dir.join("sq/b.txt"), "b\n").unwrap();
    in_sq(&["add", "b.txt"]);
    in_sq(&["commit", "-q", "-m", "b"]);
    git(&["merge", "-q", "--squash", "kraal/sq"]);
    git(&["commit", "-q", "-m", "sq (#1)"]);
    std::fs::write(dir.join("data/dev.db"), "rows").unwrap();
    // A broken setup file: the worktree is still made without it.
    std::fs::create_dir_all(repo.join(".kraal")).unwrap();
    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = 1\n").unwrap();
    git(&["add", ".kraal"]);
    git(&["commit", "-q", "-m", "broken"]);
    let out = wt(&["new", "broken"]);
    assert!(!out.status.success() && text(&out).contains(".kraal/worktree.toml"), "{}", text(&out));
    let out = wt(&["new", "broken", "--no-setup"]);
    assert!(out.status.success(), "{}", text(&out));

    let out = wt(&["prune", "--dry-run"]);
    let said = text(&out);
    assert!(said.contains("kraal/sq (its changes are in its base)"), "{said}");
    assert!(said.contains("kept data: it has ignored files not in the main checkout (dev.db)"), "{said}");
    assert!(said.contains("--discard-ignored"), "{said}");
    let out = wt(&["prune"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.join("sq").exists() && !dir.join("broken").exists());
    assert_eq!(std::fs::read_to_string(dir.join("data/dev.db")).unwrap(), "rows", "its own files are kept");
    let out = wt(&["rm", "data"]);
    assert!(!out.status.success() && text(&out).contains("--discard-ignored"), "{}", text(&out));
    let out = wt(&["prune", "--discard-ignored"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.join("data").exists());
}
