//! The files of `examples/` given to the program, each where a user would put it: read by
//! `kraal config check`, opened in a session, listed, planned, run. The directory is walked, so
//! an example added there is tested without being named here. The command lines and keys the
//! examples quote are checked with those of the pages, in `src/cli/docs.rs`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{Env, claude_hook, git_repo, wait_until, write_script};

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples")
}

/// The files under `dir`, hidden ones too, as paths from `dir`, in order.
fn files(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, from: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, from, out);
            } else {
                out.push(path.strip_prefix(from).unwrap().to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn stem(file: &Path) -> &str {
    file.file_stem().and_then(|name| name.to_str()).unwrap()
}

fn json(text: &str) -> Vec<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(text).unwrap().as_array().unwrap().clone()
}

/// A session's directories with the examples where a user copies them: the workspaces, the
/// runbooks, servers.toml and accounts.toml. Each file of `config/` is a config.toml of its own.
fn installed(name: &str) -> Env {
    let env = Env::new(name);
    for file in files(&examples()) {
        let own_place = file.starts_with("workspaces") || file.starts_with("runbooks");
        if own_place || file == Path::new("servers.toml") || file == Path::new("accounts.toml") {
            let to = env.root.join("config/kraal").join(&file);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(examples().join(&file), to).unwrap();
        }
    }
    env
}

/// A program `name` first in the `PATH` of what [`kraal`] runs: it notes how it was called and
/// where, then does `then`.
fn fake(env: &Env, name: &str, then: &str) {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let noted = format!("echo \"{name} $* in $PWD\" >> {}", env.root.join("called").display());
    write_script(&bin.join(name), &format!("#!/bin/sh\n{noted}\n{then}\n"));
}

/// What the fakes noted.
fn called(env: &Env) -> String {
    std::fs::read_to_string(env.root.join("called")).unwrap_or_default()
}

/// `program` in the test's directories, with the fakes first in its `PATH`: an example never
/// reaches the real ssh.
fn isolated(env: &Env, program: &Path) -> Command {
    let mut c = env.isolated(program);
    let path = format!("{}:{}", env.root.join("bin").display(), std::env::var("PATH").unwrap_or_default());
    c.env("PATH", path).current_dir(&env.root);
    c
}

/// kraal in the test's session, [`isolated`].
fn kraal(env: &Env, args: &[&str]) -> Command {
    let mut c = isolated(env, Path::new(env!("CARGO_BIN_EXE_kraal")));
    c.args(["-s", &env.session]).args(args);
    c
}

/// Open workspace `name` in the test's session, starting it.
fn open(env: &Env, name: &str) {
    let out = kraal(env, &["open", name]).output().unwrap();
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
}

/// What a command printed, which must succeed without a word on stderr.
fn quiet(mut command: Command) -> String {
    let out = command.output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && said.is_empty(), "{command:?} said: {said}");
    String::from_utf8(out.stdout).unwrap()
}

/// The comment a file starts with, line by line, without its marks: after the `#!` line in a
/// script, between `<!--` and `-->` in Markdown.
fn header(file: &Path, text: &str) -> Vec<String> {
    let mut lines = text.lines().peekable();
    match file.extension().and_then(|e| e.to_str()) {
        Some("md") => {
            let opened = lines.next() == Some("<!--");
            lines.take_while(|line| opened && *line != "-->").map(String::from).collect()
        }
        extension => {
            let mark = if extension == Some("kdl") { "//" } else { "#" };
            lines.next_if(|line| line.starts_with("#!"));
            lines.map_while(|line| line.strip_prefix(mark)).map(|line| line.trim().to_string()).collect()
        }
    }
}

/// Lower-case words and digits, hyphens between them.
fn kebab_case(name: &str) -> bool {
    name.split('-').all(|word| !word.is_empty() && word.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
}

/// Every example starts by saying what it is and what it needs, is named as its neighbours are,
/// and is in the index; the index lists nothing else; and nothing there is Rust, which Cargo
/// would take for an example of a library.
#[test]
fn the_directory_follows_its_rules() {
    let index = read(&examples().join("README.md"));
    let listed: Vec<&str> = index
        .split("](")
        .skip(1)
        .filter_map(|rest| rest.split(')').next())
        .filter(|link| !link.starts_with("..") && !link.contains(':'))
        .collect();
    let found: Vec<PathBuf> = files(&examples()).into_iter().filter(|file| file != Path::new("README.md")).collect();
    for file in &found {
        let name = file.display().to_string();
        assert!(file.extension().is_some_and(|e| e != "rs"), "{name}: an extension, and not Rust's");
        for part in file.iter().filter_map(|part| part.to_str()) {
            // `.kraal` is the name kraal reads in a repository.
            assert!(part.trim_start_matches('.').split('.').all(kebab_case), "{name}: {part} is not in kebab-case");
        }
        let header = header(file, &read(&examples().join(file)));
        assert!((3..=6).contains(&header.len()), "{name}: a comment of three to six lines first, not {header:?}");
        assert!(header.iter().any(|line| line.contains("Needs ")), "{name}: its comment says what it needs");
        assert!(listed.contains(&name.as_str()), "{name} is not in examples/README.md");
    }
    for link in listed {
        assert!(
            found.iter().any(|file| file == Path::new(link)),
            "examples/README.md lists {link}, which is not there"
        );
    }
}

/// `kraal config check` has nothing to say of the examples in place: the workspaces, the
/// inventory, the accounts and the runbooks, then each configuration in turn.
#[test]
fn the_check_has_nothing_to_say_of_any_example() {
    let env = installed("exchk");
    let config = env.root.join("config/kraal/config.toml");
    std::fs::remove_file(&config).unwrap();
    assert_eq!(quiet(kraal(&env, &["config", "check"])), "config ok\n");
    let configs = examples().join("config");
    for file in files(&configs) {
        std::fs::copy(configs.join(&file), &config).unwrap();
        let out = kraal(&env, &["config", "check"]).output().unwrap();
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && said.is_empty(), "config/{}: {said}", file.display());
    }
}

/// `kraal config keys` with the keys example in place: an action it gives other keys is listed
/// where the preset lists that action, with each of its keys, as text and as Markdown.
#[test]
fn the_keys_of_the_example_are_listed_in_place() {
    let env = Env::new("exkeys");
    std::fs::copy(examples().join("config/keys-classic.toml"), env.root.join("config/kraal/config.toml")).unwrap();
    let first = ["|", "\\", "-", "_", "x", "z", "space"];
    let text = quiet(kraal(&env, &["config", "keys"]));
    let rows = text.lines().skip_while(|line| *line != "After ctrl+a").skip(1);
    let keys: Vec<&str> = rows.map_while(|row| row.trim().strip_prefix("ctrl+a ")?.split(' ').next()).collect();
    assert_eq!(keys[..first.len()], first, "{text}");
    let sheet = quiet(kraal(&env, &["config", "keys", "--markdown"]));
    let rows =
        sheet.lines().skip_while(|line| *line != "## After ctrl+a").filter_map(|row| row.strip_prefix("| `ctrl+a "));
    let keys: Vec<&str> = rows.filter_map(|row| row.split("` |").next()).collect();
    assert_eq!(keys[..first.len()], ["\\|", "\\", "-", "_", "x", "z", "space"], "{sheet}");
}

/// What kraal writes to the example inventory and accounts, it edits in: the rest is the
/// example as it is, its comments included.
#[test]
fn the_files_kraal_writes_to_keep_their_text() {
    let env = installed("exkeep");
    let config = env.root.join("config/kraal");
    assert!(kraal(&env, &["servers", "add", "ops@backup.example.com"]).output().unwrap().status.success());
    let added = "\n[[server]]\nname = \"backup\"\nhost = \"backup.example.com\"\nuser = \"ops\"\n";
    assert_eq!(read(&config.join("servers.toml")), format!("{}{added}", read(&examples().join("servers.toml"))));
    assert!(kraal(&env, &["accounts", "limits", "--threshold", "70"]).output().unwrap().status.success());
    let example = read(&examples().join("accounts.toml"));
    assert_eq!(read(&config.join("accounts.toml")), example.replace("threshold = 85", "threshold = 70"));
    assert_ne!(example, example.replace("threshold = 85", "threshold = 70"));
}

/// Every workspace opens with the tabs and panes it declares, its programs started.
#[test]
fn every_workspace_opens_as_it_is_written() {
    let env = installed("exopen");
    for program in ["ssh", "claude", "make"] {
        fake(&env, program, "exec sleep 600");
    }
    // Codex is also asked for its usage, which this one does not tell.
    fake(&env, "codex", "[ \"$1\" = app-server ] && exit 1\nexec sleep 600");
    for checkout in ["home/src/shop", "home/src/shop-docs"] {
        std::fs::create_dir_all(env.root.join(checkout)).unwrap();
    }
    let workspaces = |env: &Env| json(&quiet(kraal(env, &["workspaces", "--json"])));
    let named = |list: &[serde_json::Value], name: &str| list.iter().find(|w| w["name"] == name).cloned().unwrap();
    let declared = workspaces(&env);
    for file in files(&examples().join("workspaces")) {
        let name = stem(&file);
        open(&env, name);
        let (written, opened) = (named(&declared, name), named(&workspaces(&env), name));
        assert_eq!(opened["open"], true, "{name}");
        assert_eq!((&opened["tabs"], &opened["panes"]), (&written["tabs"], &written["panes"]), "{name}");
    }
    let panes = |workspace: &str| -> Vec<serde_json::Value> {
        let all = json(&quiet(kraal(&env, &["panes", "--json"])));
        all.into_iter().filter(|pane| pane["workspace"] == workspace).collect()
    };
    let names = |panes: &[serde_json::Value]| -> Vec<String> {
        panes.iter().map(|pane| pane["name"].as_str().unwrap_or_default().to_string()).collect()
    };

    let first = panes("first");
    assert_eq!(names(&first), ["shell", "notes", "disk", "shell"]);
    assert_eq!((first[1]["kind"].as_str(), first[1]["tab"].as_str()), (Some("preview:markdown"), Some("notes")));
    assert_eq!(first[2]["command"], serde_json::json!(["df", "-h"]));

    let fleet = panes("fleet");
    assert_eq!(names(&fleet), ["web-staging-1", "web-staging-2", "web-prod-1", "shell"]);
    assert!(fleet[..3].iter().all(|pane| pane["command"][0] == "ssh"), "{fleet:?}");
    // The inventory's entries name the servers, and the one named for production is.
    wait_until(Duration::from_secs(10), "web-prod-1 is never production", || {
        panes("fleet")[2]["production"] == "server web-prod-1"
    });
    let fleet = panes("fleet");
    assert_eq!(fleet[0]["server"], "web-staging-1");
    assert!(fleet[0]["production"].is_null() && fleet[1]["production"].is_null(), "{fleet:?}");
    // Their group is the one their entries give: the workspace adds none to the inventory's.
    let group = |name: &str| -> Vec<String> {
        let members = json(&quiet(kraal(&env, &["servers", "list", "--group", name, "--json"])));
        members.iter().map(|server| server["name"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(group("web"), ["web-staging-1", "web-staging-2", "web-prod-1"]);
    assert!(group("fleet").is_empty());
}

/// Every runbook is listed as what it is, and every steps runbook shows its plan: on a group
/// of the example inventory when it runs on servers.
#[test]
fn the_runbooks_are_listed_and_planned() {
    let env = installed("exrun");
    fake(&env, "ssh", "exit 255");
    open(&env, "first");
    let inventory: toml::Table = read(&examples().join("servers.toml")).parse().unwrap();
    let servers = inventory["server"].as_array().unwrap();
    let group = servers.iter().find_map(|server| server.get("groups")?.get(0)?.as_str()).expect("a group");
    let listed = json(&quiet(kraal(&env, &["runbook", "list", "--json"])));
    let runbooks = examples().join("runbooks");
    for file in files(&runbooks) {
        let name = stem(&file);
        let steps = file.extension().is_some_and(|e| e == "toml");
        let kind = if steps { "steps" } else { "markdown" };
        assert!(listed.iter().any(|r| r["name"] == name && r["kind"] == kind), "{name} as {kind}: {listed:?}");
        if !steps {
            continue;
        }
        let runbook: toml::Table = read(&runbooks.join(&file)).parse().unwrap();
        let target = format!("@{group}");
        let mut args = vec!["runbook", "run", name, "--dry-run"];
        if runbook.get("on").and_then(|on| on.as_str()) == Some("server") {
            args.extend(["--on", &target]);
        }
        let plan = quiet(kraal(&env, &args));
        for step in runbook["steps"].as_array().unwrap() {
            let line = step.as_str().unwrap().lines().next().unwrap();
            assert!(plan.contains(line), "{name}: its plan lacks {line:?}:\n{plan}");
        }
    }
    let plan = quiet(kraal(&env, &["runbook", "run", "reload-nginx", "--on", "@web", "--dry-run", "-p", "SITE=blog"]));
    let servers = "3 servers (web-staging-1, web-staging-2, web-prod-1), one at a time, stopping at the first failure";
    assert!(plan.contains(servers) && plan.contains("PRODUCTION: web-prod-1") && plan.contains("SITE=blog"), "{plan}");
    assert_eq!(called(&env), "", "a plan connects nowhere");
}

/// What a repository commits, in a repository: the check reads it, its runbooks are listed as
/// the repository's, and a worktree is prepared as its file says once that file is approved.
#[test]
fn the_repository_example_prepares_a_worktree() {
    let env = Env::new("exrepo");
    let example = examples().join("repository");
    let committed: Vec<(String, String)> =
        files(&example).iter().map(|file| (file.display().to_string(), read(&example.join(file)))).collect();
    let committed: Vec<(&str, &str)> = committed.iter().map(|(file, text)| (file.as_str(), text.as_str())).collect();
    let repo = env.root.join("shop");
    let _git = git_repo(&repo, &committed);
    let in_repo = |args: &[&str]| {
        let mut c = kraal(&env, args);
        c.current_dir(&repo);
        c
    };
    assert_eq!(quiet(in_repo(&["config", "check"])), "config ok\n");
    let listed = json(&quiet(in_repo(&["runbook", "list", "--json"])));
    for file in files(&example.join(".kraal/runbooks")) {
        let name = stem(&file);
        assert!(listed.iter().any(|r| r["name"] == name && r["repository"].is_string()), "{name}: {listed:?}");
    }

    // What the file copies is in the main checkout only, and what it runs is a fake.
    let setup: toml::Table = read(&example.join(".kraal/worktree.toml")).parse().unwrap();
    let strings =
        |key: &str| -> Vec<&str> { setup[key].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect() };
    let copied: Vec<&str> = strings("copy").into_iter().filter(|path| !path.contains(['*', '?'])).collect();
    assert!(!copied.is_empty() && !strings("setup").is_empty());
    for path in &copied {
        std::fs::write(repo.join(path), "local\n").unwrap();
    }
    for command in strings("setup") {
        fake(&env, command.split(' ').next().unwrap(), "");
    }
    // Nobody to ask: nothing is made until the file is approved, which `--yes` does not do.
    let out = in_repo(&["wt", "new", "fix-login", "--yes"]).output().unwrap();
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let shown = quiet(in_repo(&["wt", "setup"]));
    assert!(strings("setup").iter().all(|command| shown.contains(command)), "{shown}");
    let worktree = env.root.join("shop.worktrees/fix-login");
    assert!(!worktree.exists() && called(&env).is_empty());

    quiet(in_repo(&["wt", "trust"]));
    let out = in_repo(&["wt", "new", "fix-login", "--yes"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    for path in copied {
        assert_eq!(read(&worktree.join(path)), "local\n", "{path} is copied");
    }
    let ran: Vec<String> = called(&env).lines().map(String::from).collect();
    assert_eq!(ran.len(), strings("setup").len(), "{ran:?}");
    for (ran, command) in ran.iter().zip(strings("setup")) {
        assert!(ran.starts_with(command) && ran.ends_with("shop.worktrees/fix-login"), "{command}: {ran}");
    }
}

/// The scripts are shell a `sh` reads, and notify-blocked.sh tells of an agent that waits:
/// with a fake curl, in a session whose pane a hook says is blocked.
#[test]
fn the_scripts_are_shell_and_one_tells_of_a_blocked_agent() {
    use std::os::unix::fs::PermissionsExt;
    let scripts = examples().join("scripts");
    for file in files(&scripts) {
        let path = scripts.join(&file);
        assert!(read(&path).starts_with("#!/bin/sh\n"), "{}", file.display());
        assert_ne!(path.metadata().unwrap().permissions().mode() & 0o111, 0, "{} can be run", file.display());
        let read_by_sh = Command::new("sh").arg("-n").arg(&path).output().unwrap();
        assert!(read_by_sh.status.success(), "{}: {}", file.display(), String::from_utf8_lossy(&read_by_sh.stderr));
    }

    let jq = Command::new("jq").arg("--version").output();
    assert!(jq.is_ok_and(|out| out.status.success()), "notify-blocked.sh needs jq, and so does this test");
    let env = Env::new("exnote");
    std::fs::write(env.root.join("config/kraal/workspaces/w.kdl"), "layout {\n    pane name=\"api\"\n}\n").unwrap();
    fake(&env, "curl", "");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kraal"), env.root.join("bin/kraal")).unwrap();
    open(&env, "w");
    let mut script = isolated(&env, &scripts.join("notify-blocked.sh"));
    let mut script = script.arg(&env.session).env("NTFY_TOPIC", "kraal-test").spawn().unwrap();
    // The script tells of what happens once it listens: the agent stops and asks in turn until
    // it is told of one of the two.
    let mut asks = false;
    wait_until(Duration::from_secs(20), "the script never called curl", || {
        asks = !asks;
        let event = if asks { r#""Notification","notification_type":"permission_prompt""# } else { r#""Stop""# };
        claude_hook(&env, "api", &format!(r#"{{"hook_event_name":{event},"session_id":"s-1"}}"#));
        std::thread::sleep(Duration::from_millis(100));
        !called(&env).is_empty()
    });
    let told = called(&env);
    let told = told.lines().next().unwrap();
    assert!(told.contains("Title: claude ") && told.contains("-d api in w https://ntfy.sh/kraal-test"), "{told}");
    wait_until(Duration::from_secs(5), "no cursor kept", || {
        let cursor =
            std::fs::read_to_string(env.root.join("state/kraal").join(format!("notify-blocked-{}", env.session)));
        cursor.is_ok_and(|kept| kept.split_whitespace().filter(|n| n.parse::<u64>().is_ok()).count() == 2)
    });
    // It ends with the session.
    env.run(&["kill"]);
    wait_until(Duration::from_secs(10), "the script outlives the session", || script.try_wait().unwrap().is_some());
}
