//! Sessions and workspaces: attach, list, open, save, stop.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Session;
use crate::protocol::{self, Request, Response};
use crate::{client, config, paths, workspace};

pub(super) fn attach(session: &str, workspace: Option<String>) -> Result<()> {
    let cfg = config::Config::load();
    // A server's name where a workspace's goes: said without starting a session for it, as its
    // server would say it.
    if let (Some(name), Ok(cfg)) = (&workspace, &cfg)
        && let Some((unknown, true)) = unknown_without_a_session(session, name, cfg)
    {
        bail!(unknown);
    }
    let mouse = cfg.unwrap_or_default().mouse;
    client::attach(client::Target::Local { session: session.to_string() }, workspace, mouse, || {})
}

/// What to say of `name`, given where a workspace's name goes, and whether it reads as a
/// server's, when no workspace can have that name and no session is there to say so: none runs
/// and none is saved, so every workspace would come from a definition, and none defines it.
/// `None` otherwise: the session's server knows its workspaces.
fn unknown_without_a_session(session: &str, name: &str, cfg: &config::Config) -> Option<(String, bool)> {
    // A runtime directory kraal refuses is refused where the session is reached, and comes
    // first: no socket is looked for in it here.
    paths::private_runtime_dir().ok()?;
    let session_there = std::os::unix::net::UnixStream::connect(paths::socket_path(session)).is_ok()
        || paths::session_state_path(session).exists()
        || paths::legacy_session_state_path(session).exists();
    let dirs = cfg.workspace_dirs();
    let defined =
        || dirs.iter().flat_map(|dir| workspace::load_dir(dir)).any(|(_, def)| def.is_ok_and(|def| def.name == name));
    if session_there || defined() {
        return None;
    }
    let names_server = crate::servers::names_server(&super::known_servers(cfg).unwrap_or_default(), name);
    Some((workspace::unknown(name, &dirs, names_server), names_server))
}

/// `kraal kill`: stop the session's server (its layout is saved first).
pub(super) fn kill(s: &Session) -> Result<()> {
    match client::kill_server(&paths::socket_path(&s.name))? {
        client::Saved::Yes => println!("stopped session {} (layout saved)", s.name),
        client::Saved::Unknown => println!("stopped session {}", s.name),
        client::Saved::No(why) => bail!("stopped session {}, but {why}", s.name),
    }
    Ok(())
}

/// `kraal forget`: delete what a stopped session saved.
pub(super) fn forget(s: &Session) -> Result<()> {
    let session = &s.name;
    // Not while it is restarted: it has no server for a moment then, and the next one is about
    // to restore what would be deleted here.
    let _not_restarting = client::keep_from_restart(session)?;
    // Held while the files go, so that no server starts on them meanwhile.
    let Some(_held) = paths::SessionLock::acquire(session).context("locking the session")? else {
        let who = client::who_holds(session).map(|who| format!(" ({who})")).unwrap_or_default();
        bail!("session {session} is running{who}; `kraal kill` it first");
    };
    // A server kraal 0.2.0 started holds no lock, but listens: one that takes no connection for
    // now counts as much as one that answers.
    if !paths::nobody_listens(&paths::socket_path(session)) {
        bail!("session {session} is running; `kraal kill` it first");
    }
    let p = paths::session_state_path(session);
    let legacy_path = paths::legacy_session_state_path(session);
    let legacy = remove_saved(&legacy_path)?;
    let state = remove_saved(&p)?;
    let legacy_scrollback_path = paths::state_dir().join("scrollback").join(session);
    let legacy_scrollback = match std::fs::remove_dir_all(&legacy_scrollback_path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    let scrollback = crate::persist::forget_scrollback(session)?;
    let buffers_path = paths::buffers_path(session);
    let buffers = remove_saved(&buffers_path)?;
    for (removed, path) in [
        (legacy, legacy_path),
        (state, p),
        (legacy_scrollback, legacy_scrollback_path),
        (scrollback, paths::scrollback_dir(session)),
        (buffers, buffers_path),
    ] {
        if removed {
            println!("forgot {}", path.display());
        }
    }
    if !state && !legacy && !scrollback && !legacy_scrollback && !buffers {
        println!("nothing saved for {session}");
    }
    Ok(())
}

/// Delete `path`, and the copies of it that writes killed midway left: whether it was there.
fn remove_saved(path: &Path) -> std::io::Result<bool> {
    paths::remove_abandoned_temps(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `kraal workspaces`: saved workspaces and file definitions, also when the server is stopped.
pub(super) fn workspaces(s: &Session, json: bool) -> Result<()> {
    let r = match s.request(Request::ListWorkspaces) {
        Ok(r) => r,
        Err(_) => return list_definitions(&s.name, json),
    };
    let Response::Workspaces(list) = r else { bail!("unexpected answer") };
    print_workspaces(&list, json)
}

fn print_workspaces(list: &[protocol::WorkspaceInfo], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
        return Ok(());
    }
    for w in list {
        let state = if w.open {
            "open"
        } else if w.id.is_some() {
            "closed"
        } else {
            "-"
        };
        let agents = if w.blocked + w.done + w.working > 0 {
            format!("  ▲{} ✓{} ●{}", w.blocked, w.done, w.working)
        } else {
            String::new()
        };
        println!("{:<20} {:<6} {:>3} tabs {:>4} panes{agents}", w.name, state, w.tabs, w.panes);
    }
    Ok(())
}

pub(super) fn open(s: &Session, name: String) -> Result<()> {
    // No session is started only to say that the name is unknown.
    if let Ok(cfg) = config::Config::load()
        && let Some((unknown, _)) = unknown_without_a_session(&s.name, &name, &cfg)
    {
        bail!(unknown);
    }
    client::ensure_server(&s.name)?;
    s.request(Request::OpenWorkspace { name: name.clone() })?;
    println!("workspace {name} is open; `kraal attach {name}` to see it");
    Ok(())
}

/// `kraal save`: the workspace named, else the one of the pane this runs in, else (outside a
/// pane) the one on screen, which the server knows.
pub(super) fn save(s: &Session, name: Option<String>, workspace: Option<String>) -> Result<()> {
    let workspace = workspace.or_else(|| s.own_workspace());
    if let Response::Path(p) = s.request(Request::SaveWorkspace { workspace, name })? {
        eprintln!("Workspaces are saved automatically. `kraal save` exports a KDL layout.");
        println!("saved {}", p.display());
    }
    Ok(())
}

fn confirm_layout(question: &str, next: &str) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    anyhow::ensure!(std::io::stdin().is_terminal(), "{next}");
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

pub(super) fn import(s: &Session, file: PathBuf, name: Option<String>, yes: bool) -> Result<()> {
    let path = paths::expand_tilde(&file.to_string_lossy());
    let path = if path.is_absolute() { path } else { std::env::current_dir()?.join(path) };
    let path = workspace::definition_path(&path);
    // Invalid files never start a server just to report a parser error.
    workspace::ImportPreview::read(&path)?;
    client::ensure_server(&s.name)?;
    let Response::ImportPreview(preview) =
        s.request(Request::ImportWorkspace { path: path.clone(), name: None, expected: None, accounts: Vec::new() })?
    else {
        bail!("unexpected answer")
    };
    let name = name.unwrap_or_else(|| preview.definition.name.clone());
    println!("{}\nName: {name}\n{}", path.display(), preview.summary());
    let mut accounts = Vec::new();
    for missing in &preview.accounts {
        use std::io::{IsTerminal, Write};
        anyhow::ensure!(
            !missing.choices.is_empty(),
            "Add a {} account with `kraal accounts add {}`, then retry. No workspace was added.",
            missing.agent,
            missing.agent
        );
        anyhow::ensure!(
            !yes && std::io::stdin().is_terminal(),
            "Account {} is unavailable. Run `kraal import` in a terminal without --yes to choose an account. No workspace was added.",
            missing.reference
        );
        println!("Choose a {} account for {}:", missing.agent, missing.reference);
        for (i, (_, name)) in missing.choices.iter().enumerate() {
            println!("{}. {name}", i + 1);
        }
        eprint!("Choose a number (Enter cancels): ");
        std::io::stderr().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim().is_empty() {
            return Ok(());
        }
        let index = line
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|i| i.checked_sub(1))
            .filter(|&i| i < missing.choices.len())
            .context("No account chosen. Retry and choose a listed number.")?;
        accounts.push((missing.reference.clone(), missing.choices[index].0.clone()));
    }
    if !yes
        && !confirm_layout(
            "Add workspace?",
            "No workspace was added. Run in a terminal to confirm, or use --yes to accept the preview.",
        )?
    {
        return Ok(());
    }
    s.request(Request::ImportWorkspace { path, name: Some(name.clone()), expected: Some(preview.revision), accounts })?;
    println!("Workspace {name} added. Commands wait for you to start them.");
    Ok(())
}

pub(super) fn export(s: &Session, file: PathBuf, workspace: Option<String>, replace: bool) -> Result<()> {
    let path = paths::expand_tilde(&file.to_string_lossy());
    let path = if path.is_absolute() { path } else { std::env::current_dir()?.join(path) };
    let workspace = workspace.or_else(|| s.own_workspace());
    let Response::ExportPreview { identity, path, revision, question } =
        s.request(Request::ExportWorkspace { workspace, identity: None, path, expected: None })?
    else {
        bail!("unexpected answer")
    };
    if let Some(question) = question {
        println!("{question}");
        if !replace
            && !confirm_layout(
                "Replace file?",
                "File unchanged. Choose another file or use --replace to keep a backup and replace it.",
            )?
        {
            return Ok(());
        }
    }
    if let Response::Path(path) = s.request(Request::ExportWorkspace {
        workspace: None,
        identity: Some(identity),
        path,
        expected: Some(revision),
    })? {
        println!("Layout exported\n{}", path.display());
    }
    Ok(())
}

/// The sessions whose server listens, with its socket.
pub(super) fn live_sockets() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    // Sessions an older kraal started may still listen where it put them. Both places can be
    // the same directory (macOS without XDG_RUNTIME_DIR): a session is listed once.
    let dirs = std::iter::once(paths::runtime_dir()).chain(paths::legacy_runtime_dir());
    for rd in dirs.filter_map(|d| std::fs::read_dir(d).ok()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "sock")
                && let Some(stem) = p.file_stem()
                && std::os::unix::net::UnixStream::connect(&p).is_ok()
            {
                // A long session name has a shortened socket name: the saved session says which.
                let stem = stem.to_string_lossy();
                let name = paths::session_of_socket(&stem).unwrap_or_else(|| stem.into_owned());
                if !out.iter().any(|(n, _)| *n == name) {
                    out.push((name, p.clone()));
                }
            }
        }
    }
    out
}

pub(super) fn list_sessions() -> Result<()> {
    let mut names = std::collections::BTreeMap::<String, String>::new();
    for (name, socket) in live_sockets() {
        let state = match client::ping(&socket) {
            Ok(client::ServerInfo { version, .. }) if protocol::newer(protocol::KRAAL_VERSION, &version) => {
                format!("running  {version} ({} is installed: kraal -s {name} restart)", protocol::KRAAL_VERSION)
            }
            Ok(client::ServerInfo { version, .. }) => format!("running  {version}"),
            Err(_) => "running".into(),
        };
        names.insert(name, state);
    }
    for name in paths::saved_sessions().context("reading saved sessions")? {
        names.entry(name).or_insert_with(|| "saved".into());
    }
    if names.is_empty() {
        println!("no sessions");
    }
    for (n, s) in names {
        println!("{n:<20} {s}");
    }
    Ok(())
}

fn list_definitions(session: &str, json: bool) -> Result<()> {
    let cfg = config::Config::load()?;
    let (saved, removed, order) = crate::persist::inspect(session)?
        .map(|s| (s.workspaces, s.removed_sources, s.workspace_order))
        .unwrap_or_default();
    let mut list: Vec<_> = saved
        .iter()
        .map(|w| {
            let (tabs, panes) = w.counts();
            protocol::WorkspaceInfo {
                id: (!w.id.is_empty()).then(|| w.id.clone()),
                name: w.name.clone(),
                open: false,
                definition: w.definition.clone(),
                tabs,
                panes,
                working: 0,
                blocked: 0,
                done: 0,
            }
        })
        .collect();
    let defs: Vec<_> = cfg.workspace_dirs().iter().flat_map(|d| workspace::load_dir(d)).collect();
    for (path, def) in defs {
        if removed.iter().any(|p| paths::same_file(p, &path))
            || saved.iter().any(|w| w.definition.as_deref().is_some_and(|p| paths::same_file(p, &path)))
        {
            continue;
        }
        match def {
            Ok(def) => {
                let panes = def.panes().len();
                list.push(protocol::WorkspaceInfo {
                    id: None,
                    name: def.name,
                    open: false,
                    definition: Some(path),
                    tabs: def.tabs.len(),
                    panes,
                    working: 0,
                    blocked: 0,
                    done: 0,
                });
            }
            Err(e) if !json => println!("{:<20} error: {e:#}", path.display()),
            Err(_) => {}
        }
    }
    list.sort_by_key(|w| crate::persist::WorkspaceKey::position(&order, w.id.as_deref(), w.definition.as_deref()));
    print_workspaces(&list, json)
}

fn publish_import(s: &Session, path: PathBuf, source: String, expected: paths::FileRevision) -> Result<()> {
    let _not_restarting = client::keep_from_restart(&s.name)?;
    if let Some(_held) = paths::SessionLock::acquire(&s.name).context("locking the session")?
        && paths::nobody_listens(&s.socket())
    {
        let saved = crate::persist::inspect(&s.name)?;
        let sources =
            saved.iter().flat_map(|s| &s.workspaces).filter_map(|w| Some((w.name.as_str(), w.definition.as_deref()?)));
        return workspace::publish_import(&path, &source, &expected, sources);
    }
    match s.request(Request::PublishWorkspaceFile { path, source, expected })? {
        Response::Ok => Ok(()),
        _ => bail!("unexpected answer"),
    }
}

pub(super) fn import_zellij(s: &Session, from: Option<PathBuf>, force: bool) -> Result<()> {
    let from = from.unwrap_or_else(|| paths::expand_tilde("~/.config/zellij/layouts"));
    let to = paths::workspaces_dir();
    std::fs::create_dir_all(&to)?;
    let mut n = 0;
    for (path, def) in workspace::load_dir(&from) {
        let name = path.file_name().unwrap_or_default();
        match def {
            Err(e) => eprintln!("skipped {}: {e:#}", path.display()),
            Ok(_) => {
                let dest = to.join(name);
                if dest.exists() && !force {
                    eprintln!("skipped {} ({} exists; --force to overwrite)", path.display(), dest.display());
                    continue;
                }
                let preview = workspace::ImportPreview::read(&path)?;
                let revision = paths::FileRevision::read(&dest)?;
                if revision.bytes.is_some() && !force {
                    eprintln!("skipped {} (destination appeared; retry with --force to replace it)", path.display());
                    continue;
                }
                for warning in &preview.definition.warnings {
                    eprintln!("{}: {warning}", path.display());
                }
                println!(
                    "Preview: {} ({} tabs, {} panes)",
                    path.display(),
                    preview.definition.tabs.len(),
                    preview.definition.panes().len()
                );
                let source = String::from_utf8(preview.revision.bytes.unwrap())?;
                if let Err(e) = publish_import(s, dest.clone(), source, revision) {
                    eprintln!("skipped {} ({e:#})", path.display());
                    continue;
                }
                println!(
                    "{} → {} ({} tabs, {} panes)",
                    path.display(),
                    paths::shorten_home(&dest),
                    preview.definition.tabs.len(),
                    preview.definition.panes().len()
                );
                n += 1;
            }
        }
    }
    println!("imported {n} workspace{}", if n == 1 { "" } else { "s" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_publication_cannot_bypass_a_held_session_lock() {
        let name = format!("batch-publication-{}", std::process::id());
        let session = Session { explicit: Some(name.clone()), name };
        let dir = std::env::temp_dir().join(format!("kraal-{}", session.name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("copy.kdl");
        let original = "layout { pane command=\"true\"; }";
        let replacement = "layout { pane command=\"false\"; }";
        std::fs::write(&path, original).unwrap();
        let expected = paths::FileRevision::read(&path).unwrap();
        let held = paths::SessionLock::acquire(&session.name).unwrap().unwrap();
        assert!(publish_import(&session, path.clone(), replacement.into(), expected.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_extension("kdl.bak").exists());
        drop(held);
        publish_import(&session, path.clone(), replacement.into(), expected).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
        assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
