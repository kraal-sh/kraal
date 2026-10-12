//! Agents from the command line: accounts, launching, worktrees and the hooks integration.

use anyhow::{Context, Result, bail};

use super::Session;
use super::args::{AccountsCmd, WtCmd};
use crate::protocol::{PaneInfo, Placement, Request, Response};
use crate::{accounts, codex_usage, git, integration, paths, server, worktree_setup};

pub(super) fn accounts(action: AccountsCmd) -> Result<()> {
    let all = accounts::Accounts::load()?;
    match action {
        AccountsCmd::List { json } => {
            let list = all.all();
            let now = accounts::now();
            let usage = accounts::UsageMap::load(&list);
            // Ask every agent in parallel whether its account is logged in.
            let status: Vec<Option<bool>> = std::thread::scope(|s| {
                let handles: Vec<_> = list.iter().map(|a| s.spawn(move || logged_in(a))).collect();
                handles.into_iter().map(|h| h.join().ok().flatten()).collect()
            });
            if json {
                let v: Vec<serde_json::Value> = list
                    .iter()
                    .zip(&status)
                    .map(|(a, st)| {
                        serde_json::json!({
                            "agent": a.agent, "name": a.name, "home": a.home, "logged_in": st,
                            "threshold": all.threshold(a), "usage": usage.get(a),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for (a, st) in list.iter().zip(&status) {
                let login = match st {
                    Some(true) => "logged in",
                    Some(false) => "not logged in",
                    None => "agent not found",
                };
                let u = usage.get(a).map(|u| server::usage_text(u, now)).unwrap_or_else(|| "no usage seen yet".into());
                println!(
                    "{:<18} {:<14} {:<34} {u}",
                    accounts::label(a),
                    login,
                    paths::shorten_home(&paths::expand_tilde(&a.home))
                );
            }
            println!("threshold {}% · when reached: {:?}", all.limits.threshold, all.limits.mode);
            Ok(())
        }
        AccountsCmd::Add { agent, name, home, threshold, no_login } => {
            let home = home.unwrap_or_else(|| accounts::default_home_for(&agent, &name).display().to_string());
            let a = accounts::Account { name, agent, home: accounts::absolute_home(&home)?, threshold, note: None };
            let notes = accounts::Accounts::update(|all| {
                all.add(a.clone())?;
                accounts::prepare_home(&a)
            })?;
            for note in notes {
                println!("  {note}");
            }
            println!("added {} ({})", accounts::label(&a), paths::shorten_home(&paths::expand_tilde(&a.home)));
            if no_login {
                return Ok(());
            }
            println!("logging in with {}'s own login…", a.agent);
            login(&a)
        }
        AccountsCmd::Login { account } => {
            let a = find_account(&all, &account)?;
            login(&a)
        }
        AccountsCmd::Rm { account } => {
            let a = find_account(&all, &account)?;
            accounts::Accounts::update(|all| {
                if !all.remove(&a.agent, &a.name) {
                    bail!("{} is the agent's default account; it cannot be forgotten", accounts::label(&a));
                }
                Ok(())
            })?;
            println!("forgot {} (its directory {} is kept)", accounts::label(&a), a.home);
            Ok(())
        }
        AccountsCmd::Rename { account, name } => {
            let a = find_account(&all, &account)?;
            accounts::rename(&a.agent, &a.name, &name)?;
            println!("renamed {} to {}·{name}", accounts::label(&a), a.agent);
            Ok(())
        }
        AccountsCmd::Refresh => {
            for a in all.for_agent("codex") {
                let home = paths::expand_tilde(&a.home);
                match codex_usage::fetch("codex", &home, std::time::Duration::from_secs(20)) {
                    Ok(l) => {
                        let conv = |w: Option<codex_usage::Window>| {
                            w.map(|w| accounts::Window { used_percent: w.used_percent, resets_at: w.resets_at })
                        };
                        let u = accounts::Usage {
                            five_hour: conv(l.five_hour),
                            weekly: conv(l.weekly),
                            observed_at: l.observed_at,
                            source: "codex".into(),
                        };
                        accounts::save_usage(&a.agent, &a.name, &u)?;
                        println!("{:<18} {}", accounts::label(&a), server::usage_text(&u, accounts::now()));
                    }
                    Err(e) => println!("{:<18} {e:#}", accounts::label(&a)),
                }
            }
            println!("Claude accounts report their usage through the status line while they run.");
            Ok(())
        }
        AccountsCmd::Limits { threshold, mode } => {
            if let Some(t) = threshold
                && !(1..=100).contains(&t)
            {
                bail!("the threshold is a percentage (1-100)");
            }
            let limits = accounts::Accounts::update(|all| {
                if let Some(t) = threshold {
                    all.limits.threshold = t;
                }
                if let Some(m) = &mode {
                    all.limits.mode = match m.as_str() {
                        "ask" => accounts::Mode::Ask,
                        "auto" => accounts::Mode::Auto,
                        _ => accounts::Mode::Notify,
                    };
                }
                Ok(all.limits.clone())
            })?;
            println!("threshold {}% · when reached: {:?}", limits.threshold, limits.mode);
            Ok(())
        }
        AccountsCmd::Edit => {
            if !accounts::Accounts::path().exists() {
                all.save()?;
            }
            let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
            let mut parts = editor.split_whitespace();
            std::process::Command::new(parts.next().unwrap_or("vi"))
                .args(parts)
                .arg(accounts::Accounts::path())
                .status()?;
            accounts::Accounts::load().map(|_| ())
        }
    }
}

/// `agent:name` or a bare account name (searched across agents).
fn find_account(all: &accounts::Accounts, spec: &str) -> Result<accounts::Account> {
    if let Some((agent, name)) = spec.split_once(':').or_else(|| spec.split_once('·')) {
        return all.get(agent, name).with_context(|| format!("{agent} has no account named {name:?}"));
    }
    let matches: Vec<accounts::Account> = all.all().into_iter().filter(|a| a.name == spec).collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no account named {spec:?} (see `kraal accounts`)"),
        _ => bail!("several agents have an account named {spec:?}; use agent:name"),
    }
}

/// The agent's own login for account `a`, in this terminal; a failed login is an error.
fn login(a: &accounts::Account) -> Result<()> {
    let status = run_with(&accounts::login(a)?, false)?;
    if !status.success() {
        bail!("{}'s login did not succeed ({status})", accounts::label(a));
    }
    Ok(())
}

/// Run a command with an account's environment in this terminal.
fn run_with(l: &accounts::Launch, exec: bool) -> Result<std::process::ExitStatus> {
    let mut cmd = std::process::Command::new(&l.argv[0]);
    cmd.args(&l.argv[1..]);
    for k in &l.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &l.env {
        cmd.env(k, v);
    }
    if exec {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        return Err(err).with_context(|| format!("running {}", l.argv[0]));
    }
    cmd.status().with_context(|| format!("running {}", l.argv[0]))
}

/// Logged in? Asks the agent itself (`claude auth status`, `codex login status`).
fn logged_in(a: &accounts::Account) -> Option<bool> {
    let l = accounts::login_status(a).ok()?;
    let mut cmd = std::process::Command::new(&l.argv[0]);
    cmd.args(&l.argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for k in &l.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &l.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status.success());
        }
        if start.elapsed() > std::time::Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
}

/// `kraal agent`: a new agent pane in the session, or the agent here.
pub(super) fn agent(
    s: &Session,
    agent: String,
    account: Option<String>,
    placement: Placement,
    here: bool,
    args: Vec<String>,
) -> Result<()> {
    let inside = std::env::var_os("KRAAL_SOCKET").is_some();
    if inside && !here {
        s.request(Request::OpenAgent {
            agent,
            account,
            placement,
            near: s.own_pane(),
            cwd: std::env::current_dir().ok(),
            args,
        })?;
        return Ok(());
    }
    // Here, in this pane: its workspace's account rules still apply.
    let workspace = if inside { own_workspace(s) } else { None };
    exec_agent(&agent, account.as_deref(), workspace.as_deref(), &args, &[])
}

/// The workspace of the pane this command runs in.
fn own_workspace(s: &Session) -> Option<String> {
    let pane = s.own_pane()?;
    let Ok(Response::Panes(list)) = s.request(Request::ListPanes) else { return None };
    list.into_iter().find(|p| p.id.to_string() == pane).map(|p| p.workspace)
}

/// Start an agent on the chosen account in this terminal, with `env` besides the account's.
fn exec_agent(
    agent: &str,
    account: Option<&str>,
    workspace: Option<&str>,
    args: &[String],
    env: &[(String, String)],
) -> Result<()> {
    let all = accounts::Accounts::load()?;
    accounts::agent_kind(agent)
        .with_context(|| format!("unknown agent {agent:?} (known: {})", accounts::agent_names()))?;
    let cwd = std::env::current_dir().ok();
    // Account rules see a worktree as its repository; never a pick among accounts a rule may
    // forbid here because git could not tell which repository that is.
    if let Some(c) = &cwd {
        git::project_path(c);
    }
    if account.is_none() && all.undecided(agent, cwd.as_deref()) {
        bail!("git did not say in time which repository this directory belongs to; try again");
    }
    let usage = accounts::UsageMap::load(&all.all());
    let a = match account {
        Some(name) => all.get(agent, name).with_context(|| format!("{agent} has no account named {name:?}"))?,
        None => all
            .choose(agent, workspace, cwd.as_deref(), &usage, accounts::now())
            .context("no account allowed here: the matching rule in accounts.toml names none")?,
    };
    let session = accounts::agent_kind(agent).and_then(|k| k.session_flag).map(|_| accounts::new_uuid());
    let mut launch = accounts::launch(&a, args, None, session.as_deref())?;
    launch.env.extend(env.iter().cloned());
    run_with(&launch, true).map(|_| ())
}

/// `kraal wt`: git worktrees for agents.
pub(super) fn worktrees(s: &Session, action: WtCmd) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match action {
        WtCmd::New { name, agent, base, yes, no_setup } => {
            use std::io::{BufRead, IsTerminal, Write};
            let cfg = crate::config::Config::load()?;
            let user = worktree_setup::Setup::of_user(&cfg.worktrees);
            let plan = worktree_setup::Plan::new(&cwd, &name, base.as_deref(), user, !no_setup)?;
            // The repository's commands run once this version of its file is approved.
            if plan.needs_trust() {
                println!("{}", plan.trust_question());
                if !yes {
                    anyhow::ensure!(
                        std::io::stdin().is_terminal(),
                        "add --yes to trust it and run it, or --no-setup to make the worktree without it"
                    );
                    eprint!("Trust this version and run it? [y/N] ");
                    std::io::stderr().flush()?;
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line)?;
                    if !matches!(line.trim(), "y" | "Y" | "yes") {
                        bail!("cancelled; nothing was created");
                    }
                }
                plan.trust()?;
            }
            let prepared = plan.create()?;
            let w = &prepared.worktree;
            println!("{} on {} (from {})", w.path.display(), w.branch, w.base);
            let env = worktree_setup::env(&w.name(), w.index, &cfg.worktrees);
            prepared.run_here(&env)?;
            if let Some(agent) = agent {
                if std::env::var_os("KRAAL_SOCKET").is_some() {
                    s.request(Request::OpenAgent {
                        agent,
                        account: None,
                        placement: Placement::Tab,
                        near: s.own_pane(),
                        cwd: Some(w.path.clone()),
                        args: Vec::new(),
                    })?;
                } else {
                    std::env::set_current_dir(&w.path)?;
                    return exec_agent(&agent, None, None, &[], &env);
                }
            }
            Ok(())
        }
        WtCmd::List => {
            for w in git::list_worktrees(&cwd)? {
                let state = git::status(&w.path).map(|g| g.label(false)).unwrap_or_default();
                let number = if w.index > 0 { format!("#{}", w.index) } else { String::new() };
                println!("{:<24} {number:<4} {:<32} {}", w.name(), state, paths::shorten_home(&w.path));
            }
            Ok(())
        }
        WtCmd::Prune { dry_run, discard_ignored } => {
            // Panes of the running session that work in a worktree keep it: close them first.
            let panes = match s.request(Request::ListPanes) {
                Ok(Response::Panes(panes)) => panes,
                _ => Vec::new(),
            };
            let discard = if discard_ignored { git::Discard::Ignored } else { git::Discard::Nothing };
            let mut failed = 0;
            for (w, state) in git::worktree_states(&cwd)? {
                let inside: Vec<String> = panes
                    .iter()
                    .filter(|p| p.cwd.as_ref().is_some_and(|c| git::is_within(c, &w.path)))
                    .map(|p| p.name.clone().unwrap_or_else(|| p.id.to_string()))
                    .collect();
                let kept = match &state {
                    Err(e) => Some(e.clone()),
                    Ok(s) if !s.is_clean() => Some(format!("it has {}", s.describe())),
                    Ok(s) if !s.ignored.is_empty() && !discard_ignored => Some(format!(
                        "it has ignored files not in the main checkout ({}); --discard-ignored removes them too",
                        s.ignored_names()
                    )),
                    Ok(_) if !inside.is_empty() => Some(format!("panes work in it ({})", inside.join(", "))),
                    Ok(_) => None,
                };
                let how = match &state {
                    Ok(s) if s.unmerged > 0 => " (its changes are in its base)",
                    _ => "",
                };
                if let Some(why) = kept {
                    println!("kept {}: {why}", w.name());
                } else if dry_run {
                    println!("would remove {} and {}{how}", w.path.display(), w.branch);
                } else {
                    // Checked again as it is removed: nothing with changes or unmerged commits goes.
                    match git::remove_worktree(&w, discard) {
                        Ok(()) => println!("removed {} and {}", w.path.display(), w.branch),
                        Err(e) => {
                            eprintln!("kept {}: {e:#}", w.name());
                            failed += 1;
                        }
                    }
                }
            }
            anyhow::ensure!(
                failed == 0,
                "{failed} worktree{} could not be removed",
                if failed == 1 { "" } else { "s" }
            );
            Ok(())
        }
        WtCmd::Rm { name, force, discard_ignored } => {
            let w = git::list_worktrees(&cwd)?
                .into_iter()
                .find(|w| w.name() == name)
                .with_context(|| format!("no kraal worktree named {name:?} here"))?;
            // Panes of the running session that work inside it go first, or stop us.
            if let Ok(Response::Panes(panes)) = s.request(Request::ListPanes) {
                let inside: Vec<&PaneInfo> =
                    panes.iter().filter(|p| p.cwd.as_ref().is_some_and(|c| git::is_within(c, &w.path))).collect();
                if !inside.is_empty() && !force {
                    let names: Vec<String> =
                        inside.iter().map(|p| p.name.clone().unwrap_or_else(|| p.id.to_string())).collect();
                    bail!(
                        "panes work in {}: {} (close them, or --force closes them)",
                        w.path.display(),
                        names.join(", ")
                    );
                }
                for p in inside {
                    s.request(Request::ClosePane { pane: p.id.to_string() })?;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            let discard = match (force, discard_ignored) {
                (true, _) => git::Discard::Everything,
                (false, true) => git::Discard::Ignored,
                (false, false) => git::Discard::Nothing,
            };
            git::remove_worktree(&w, discard).map_err(|e| match git::worktree_state(&w) {
                Ok(state) if state.is_clean() && !state.ignored.is_empty() => {
                    anyhow::anyhow!("{e:#}; --discard-ignored removes it with them")
                }
                _ => e,
            })?;
            println!("removed {} and {}", w.path.display(), w.branch);
            Ok(())
        }
    }
}

/// `kraal integration`: install, remove or check kraal's hooks in agents' settings, and the
/// lines that make shells mark their commands. `all` never edits a shell's startup file, and
/// covers the agents used on this machine besides Claude Code and Codex.
pub(super) fn integration(action: &str, agent: &str) -> Result<()> {
    let agents: Vec<&str> = match (agent, action) {
        ("all", "status") => {
            integration::agents_here().into_iter().chain(integration::SHELLS.iter().copied()).collect()
        }
        ("all", "show") => bail!("show what: zsh, bash or fish"),
        ("all", _) => integration::agents_here(),
        _ => vec![agent],
    };
    let mut failed = Vec::new();
    for a in agents {
        let r = match action {
            "install" => integration::install(a),
            "uninstall" => integration::uninstall(a),
            "show" => integration::show(a),
            _ => integration::status(a),
        };
        match r {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("{a}: {e:#}");
                failed.push(a);
            }
        }
    }
    if !failed.is_empty() {
        bail!("{action} failed for {}", failed.join(", "));
    }
    Ok(())
}
