//! `kraal runbook`: list, show, create, edit and run runbooks.

use anyhow::{Context, Result};

use super::args::RunbookCmd;
use crate::protocol::{Request, Response};
use crate::{client, git, paths, runbooks, servers};

fn read_line() -> Result<String> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}

/// A line typed without being shown (a secret parameter). Refuses when the terminal cannot hide
/// it; a signal while typing still gives the terminal its echo back.
fn read_hidden() -> Result<String> {
    let refuse = || anyhow::anyhow!("this terminal cannot hide what is typed: pass the value with -p NAME=value");
    // SAFETY: tcgetattr/tcsetattr on stdin with termios structs we own.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
        return Err(refuse());
    }
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    let mut handlers = Vec::new();
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only calls async-signal-safe functions (tcsetattr, _exit).
        let id = unsafe {
            signal_hook::low_level::register(sig, move || {
                libc::tcsetattr(0, libc::TCSANOW, &saved);
                libc::_exit(128 + sig);
            })
        };
        handlers.extend(id.ok());
    }
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) } != 0 {
        for id in handlers {
            signal_hook::low_level::unregister(id);
        }
        return Err(refuse());
    }
    let line = read_line();
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
    for id in handlers {
        signal_hook::low_level::unregister(id);
    }
    eprintln!();
    line
}

/// Ask `question` on the terminal: only a yes is one.
fn confirmed(question: &str) -> Result<bool> {
    use std::io::Write;
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    Ok(matches!(read_line()?.trim(), "y" | "Y" | "yes"))
}

/// Before the runbook `name` runs: a repository's must be approved as it is now. Approving is an
/// act of its own, which answering the plan (`--yes`) is not: where someone can `answer`, the
/// file is shown and the question asked; otherwise the run is refused, with the way to approve.
fn approve(r: &runbooks::Runbook, name: &str, answer: bool) -> Result<()> {
    let runbooks::Source::Repo(root) = &r.source else { return Ok(()) };
    if runbooks::trusted(r) {
        return Ok(());
    }
    anyhow::ensure!(answer, runbooks::not_approved(name, root));
    println!(
        "{name} comes from the repository {}. This version of it is new, or changed since you approved it:\n\n{}",
        paths::shorten_home(root),
        r.source.shown(&r.content)
    );
    if !confirmed("Trust this version?")? {
        anyhow::bail!("cancelled; {name} is not approved");
    }
    runbooks::trust(r)
}

/// The repository of the current directory, for its runbooks.
pub(super) fn current_repo() -> Option<std::path::PathBuf> {
    git::status(&std::env::current_dir().ok()?).map(|g| g.root)
}

fn find_runbook(name: &str) -> Result<runbooks::Runbook> {
    let (list, errors) = runbooks::all(current_repo().as_deref());
    for e in &errors {
        eprintln!("warning: {e}");
    }
    list.into_iter()
        .find(|r| r.name == name)
        .with_context(|| format!("no runbook {name:?} (`kraal runbook list`, or `kraal runbook new {name}`)"))
}

pub(super) fn run(action: RunbookCmd, socket: &std::path::Path, own_pane: Option<String>) -> Result<()> {
    use std::io::{IsTerminal, Write};
    match action {
        RunbookCmd::List { json } => {
            let (list, errors) = runbooks::all(current_repo().as_deref());
            for e in &errors {
                eprintln!("warning: {e}");
            }
            if json {
                let v: Vec<serde_json::Value> = list
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "name": r.name,
                            "path": r.path,
                            "kind": if matches!(r.body, runbooks::Body::Markdown { .. }) { "markdown" } else { "steps" },
                            "repository": match &r.source { runbooks::Source::Repo(root) => Some(root.clone()), _ => None },
                            "description": r.description(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            if list.is_empty() {
                println!("no runbooks yet: `kraal runbook new <name>` (or --markdown)");
            }
            for r in &list {
                let kind = match &r.body {
                    runbooks::Body::Markdown { .. } => "doc",
                    runbooks::Body::Steps(s) => match s.on {
                        runbooks::On::Server => "servers",
                        runbooks::On::Pane => "pane",
                        runbooks::On::Local => "local",
                    },
                };
                let from = match &r.source {
                    runbooks::Source::Repo(root) => format!("  [{}]", paths::shorten_home(root)),
                    runbooks::Source::User => String::new(),
                };
                let (name, description) = (r.source.shown(&r.name), r.source.shown(&r.description()));
                println!("{name:<24} {kind:<8} {description}{from}");
            }
            Ok(())
        }
        RunbookCmd::Show { name } => {
            let r = find_runbook(&name)?;
            println!("{}\n", r.path.display());
            print!("{}", r.source.shown(&r.content));
            Ok(())
        }
        RunbookCmd::Trust { name } => {
            let r = find_runbook(&name)?;
            match &r.source {
                runbooks::Source::User => println!("{name} is one of your own runbooks: it needs no approval"),
                runbooks::Source::Repo(root) => {
                    runbooks::trust(&r)?;
                    println!(
                        "{name}, from the repository {}, is approved as it is now: kraal asks again when it changes",
                        paths::shorten_home(root)
                    );
                }
            }
            Ok(())
        }
        RunbookCmd::New { name, markdown, repo } => {
            if !servers::valid_name(&name) {
                anyhow::bail!("invalid name {name:?} (letters, digits, - _ .)");
            }
            let dir = if repo {
                runbooks::repo_dir(&current_repo().context("not in a git repository")?)
            } else {
                runbooks::user_dir()
            };
            let path = dir.join(format!("{name}.{}", if markdown { "md" } else { "toml" }));
            std::fs::create_dir_all(&dir)?;
            let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    anyhow::bail!("{} already exists", path.display())
                }
                other => other?,
            };
            std::io::Write::write_all(
                &mut file,
                if markdown { runbooks::TEMPLATE_MD } else { runbooks::TEMPLATE_TOML }.as_bytes(),
            )?;
            println!("{}", path.display());
            Ok(())
        }
        RunbookCmd::Edit { name } => {
            let r = find_runbook(&name)?;
            let editor = crate::edit::editor_argv(&r.path, None);
            let status = std::process::Command::new(&editor[0]).args(&editor[1..]).status()?;
            anyhow::ensure!(status.success(), "the editor failed");
            Ok(())
        }
        RunbookCmd::Run { name, on, params, yes, dry_run, wait } => {
            let r = find_runbook(&name)?;
            let runbooks::Body::Steps(steps) = &r.body else {
                anyhow::bail!("{name} is a document: open it in kraal (ctrl+b R b) and run its steps there");
            };
            let mut values = Vec::new();
            for p in &params {
                let (k, v) = p.split_once('=').with_context(|| format!("{p:?}: use NAME=value"))?;
                values.push((k.to_string(), v.to_string()));
            }
            let interactive = std::io::stdin().is_terminal();
            // A plan runs nothing: it needs no approval.
            if !dry_run {
                approve(&r, &name, interactive && !yes)?;
            }
            // Ask for parameters that have no value and no default, when someone can answer.
            for p in &steps.params {
                if p.default.is_none() && !values.iter().any(|(k, _)| k == &p.name) && interactive {
                    eprint!("{}: ", r.source.shown(&p.prompt()));
                    std::io::stderr().flush()?;
                    let line = if p.secret { read_hidden()? } else { read_line()? };
                    values.push((p.name.clone(), line.trim().to_string()));
                }
            }
            let prepare = Request::RunbookPlan { path: r.path.clone(), target: on, params: values, pane: own_pane };
            let Response::Plan { text, token } = client::request(socket, prepare)? else {
                anyhow::bail!("unexpected answer from the server")
            };
            println!("{}", r.source.shown(&text));
            if dry_run {
                return Ok(());
            }
            if !yes {
                anyhow::ensure!(interactive, "add --yes to run it without asking");
                if !confirmed("Run it?")? {
                    anyhow::bail!("cancelled");
                }
            }
            let finished = if wait {
                let (tx, rx) = crossbeam_channel::unbounded::<serde_json::Value>();
                let (ready_tx, ready_rx) = crossbeam_channel::bounded::<()>(1);
                let sock = socket.to_path_buf();
                std::thread::spawn(move || {
                    let _ = client::subscribe(
                        &sock,
                        vec!["runbook.finished".into()],
                        None,
                        move || {
                            let _ = ready_tx.send(());
                        },
                        |ev| tx.send(ev.clone()).is_ok(),
                    );
                });
                ready_rx.recv_timeout(std::time::Duration::from_secs(5)).context("could not follow the run")?;
                Some(rx)
            } else {
                None
            };
            let Response::Text(id) = client::request(socket, Request::RunbookRun { token })? else {
                anyhow::bail!("unexpected answer from the server")
            };
            println!("started (run {id}); its panes are in the tab \"▶ {name}\"");
            let Some(rx) = finished else { return Ok(()) };
            let run: u64 = id.parse()?;
            for ev in rx {
                if ev["run"].as_u64() != Some(run) {
                    continue;
                }
                let (ok, failed, skipped) = (&ev["ok"], &ev["failed"], &ev["skipped"]);
                println!(
                    "finished: {ok} ok, {failed} failed, {skipped} skipped · log {}",
                    ev["log"].as_str().unwrap_or("")
                );
                anyhow::ensure!(*failed == 0 && *skipped == 0, "{name} did not succeed everywhere");
                return Ok(());
            }
            anyhow::bail!("the server went away before the run finished")
        }
    }
}
