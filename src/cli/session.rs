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
pub(super) fn workspaces(
    s: &Session,
    json: bool,
    deleted: bool,
    command: Option<super::args::WorkspaceCmd>,
) -> Result<()> {
    if let Some(command) = command {
        return workspace_command(s, command);
    }
    if deleted {
        let list = match s.request(Request::ListDeletedWorkspaces) {
            Ok(Response::Workspaces(list)) => list,
            Ok(_) => bail!("unexpected answer"),
            Err(error) => {
                if std::os::unix::net::UnixStream::connect(s.socket()).is_ok() {
                    return Err(error);
                }
                crate::persist::inspect(&s.name)?
                    .map(|saved| {
                        saved
                            .deleted_workspaces
                            .iter()
                            .rev()
                            .map(|d| {
                                let w = &d.workspace;
                                let (tabs, panes) = w.counts();
                                protocol::WorkspaceInfo {
                                    id: Some(w.id.clone()),
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
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            }
        };
        if json {
            println!("{}", serde_json::to_string_pretty(&list)?);
        } else {
            for w in list {
                println!("{}  {}  deleted", w.id.as_deref().unwrap_or_default(), w.name);
            }
        }
        return Ok(());
    }
    let r = match s.request(Request::ListWorkspaces) {
        Ok(r) => r,
        Err(_) => return list_definitions(&s.name, json),
    };
    let Response::Workspaces(list) = r else { bail!("unexpected answer") };
    print_workspaces(&list, json)
}

fn workspace_command(s: &Session, command: super::args::WorkspaceCmd) -> Result<()> {
    use super::args::WorkspaceCmd;
    client::ensure_server(&s.name)?;
    let (request, message) = match command {
        WorkspaceCmd::Create { name } => {
            (Request::CreateWorkspace { name, cwd: std::env::current_dir()? }, "Workspace added")
        }
        WorkspaceCmd::Rename { workspace, name } => (Request::RenameWorkspace { workspace, name }, "Workspace renamed"),
        WorkspaceCmd::Restore { workspace, name } => (
            Request::RestoreWorkspace { workspace, name },
            "Workspace restored · Closed. Use `kraal open NAME` to open it.",
        ),
        WorkspaceCmd::Close { workspace, yes } => return stop_workspace(s, workspace, yes, false),
        WorkspaceCmd::Delete { workspace, yes } => return stop_workspace(s, workspace, yes, true),
    };
    s.request(request)?;
    println!("{message}");
    Ok(())
}

fn stop_workspace(s: &Session, workspace: String, yes: bool, delete: bool) -> Result<()> {
    let request = |workspace, expected| {
        if delete {
            Request::DeleteWorkspace { workspace, expected }
        } else {
            Request::CloseWorkspace { workspace, expected }
        }
    };
    let Response::WorkspacePreview { workspace, revision, question } = s.request(request(workspace, None))? else {
        bail!("unexpected answer")
    };
    if let Some(question) = question {
        println!("{question}");
        if !yes
            && !confirm_layout(
                if delete { "Delete workspace?" } else { "Close workspace?" },
                "Nothing was stopped or deleted. Run in a terminal to confirm, or use --yes to accept the preview.",
            )?
        {
            return Ok(());
        }
    }
    s.request(request(workspace, Some(revision)))?;
    println!(
        "{}",
        if delete {
            "Workspace deleted. Use `kraal workspaces --deleted` to restore it."
        } else {
            "Workspace closed. The layout is kept."
        }
    );
    Ok(())
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
    use std::io::ErrorKind;
    use std::time::{Duration, Instant};

    let _not_restarting = client::keep_from_restart(&s.name)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let held = paths::SessionLock::acquire(&s.name).context("locking the session")?;
        let socket = s.socket();
        if let Some(_held) = held
            && paths::nobody_listens(&socket)
        {
            let saved = crate::persist::inspect(&s.name)?;
            let sources = saved
                .iter()
                .flat_map(|s| &s.workspaces)
                .filter_map(|w| Some((w.name.as_str(), w.definition.as_deref()?)));
            return workspace::publish_import(&path, &source, &expected, sources);
        }
        match paths::connect_now(&socket) {
            Ok(stream) => {
                stream.set_nonblocking(false)?;
                return match client::request_connected(
                    &socket,
                    stream,
                    Request::PublishWorkspaceFile { path, source, expected },
                )? {
                    Response::Ok => Ok(()),
                    _ => bail!("unexpected answer"),
                };
            }
            Err(e)
                if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::WouldBlock) => {}
            Err(e) => return Err(e).context("connecting to the session"),
        }
        // A lock can outlive its owner until a forked child closes the inherited descriptor.
        // A server starting or stopping also holds it without a reachable socket.
        anyhow::ensure!(
            Instant::now() < deadline,
            "session {} is busy and no server is reachable; no file was changed. Retry the import",
            s.name
        );
        std::thread::sleep(Duration::from_millis(20));
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
    fn publication_survives_a_peer_disconnect_before_writing() {
        use std::os::unix::net::UnixListener;

        if !crate::testing::in_own_process("cli::session::tests::publication_survives_a_peer_disconnect_before_writing")
        {
            return;
        }
        let dir = paths::private_runtime_dir().unwrap();
        let socket = dir.join("publication.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let stream = paths::connect_now(&socket).unwrap();
        let (peer, _) = listener.accept().unwrap();
        // The peer is gone before any request bytes are written: no scheduling race can
        // turn this into a disconnect after the complete request was sent.
        drop(peer);
        drop(listener);
        let path = dir.join("copy.kdl");
        let expected = paths::FileRevision::read(&path).unwrap();
        // SAFETY: this test runs alone in its own process. Match the import command's
        // disposition, so a socket that does not suppress SIGPIPE kills this child.
        let previous = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        assert_ne!(previous, libc::SIG_ERR);
        let result = client::request_connected(
            &socket,
            stream,
            Request::PublishWorkspaceFile { path, source: "layout { pane; }".into(), expected },
        );
        // SAFETY: restore this process's disposition after the write.
        assert_ne!(unsafe { libc::signal(libc::SIGPIPE, previous) }, libc::SIG_ERR);
        let error = result.unwrap_err();
        assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), std::io::ErrorKind::BrokenPipe);
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn batch_publication_cannot_bypass_a_held_session_lock() {
        // Another test's child can inherit the runtime lock until it reaches exec, keeping it
        // held briefly after drop. Run alone so the publication below sees the released lock.
        if !crate::testing::in_own_process("cli::session::tests::batch_publication_cannot_bypass_a_held_session_lock") {
            return;
        }
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
        let error = publish_import(&session, path.clone(), replacement.into(), expected.clone()).unwrap_err();
        assert!(error.to_string().contains("no file was changed. Retry the import"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_extension("kdl.bak").exists());
        drop(held);
        publish_import(&session, path.clone(), replacement.into(), expected).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
        assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn batch_publication_waits_for_an_inherited_runtime_lock() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let name = format!("batch-inherited-{}", std::process::id());
        let session = Session { explicit: Some(name.clone()), name };
        let dir = std::env::temp_dir().join(format!("kraal-{}", session.name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("copy.kdl");
        let original = "layout { pane command=\"true\"; }";
        let replacement = "layout { pane command=\"false\"; }";
        std::fs::write(&path, original).unwrap();
        let expected = paths::FileRevision::read(&path).unwrap();
        paths::private_runtime_dir().unwrap();
        let runtime = paths::runtime_dir().join(format!("{}.lock", session.name));
        let held = std::fs::File::create(&runtime).unwrap();
        assert!(paths::try_lock(&held).unwrap());
        // A duplicate shares the open file and its flock, just as a forked child does before
        // exec. Keep that window open until the publisher has tried to use the session.
        let inherited = held.try_clone().unwrap();
        drop(held);
        assert!(paths::SessionLock::acquire(&session.name).unwrap().is_none());
        assert!(paths::nobody_listens(&session.socket()));
        std::thread::scope(|scope| {
            let (sent, received) = mpsc::channel();
            let (session, path) = (&session, &path);
            scope.spawn(move || {
                sent.send(publish_import(session, path.clone(), replacement.into(), expected)).unwrap();
            });
            let restart = paths::state_dir().join(format!("{}.restart", session.name));
            let start = Instant::now();
            while !restart.exists() {
                assert!(
                    matches!(received.try_recv(), Err(mpsc::TryRecvError::Empty)),
                    "publication ended before waiting"
                );
                assert!(start.elapsed() < crate::testing::PATIENCE, "publication never started");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(matches!(received.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)));
            assert_eq!(std::fs::read_to_string(path).unwrap(), original);
            assert!(!path.with_extension("kdl.bak").exists());
            drop(inherited);
            received.recv_timeout(crate::testing::PATIENCE).unwrap().unwrap();
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
        assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn batch_publication_keeps_the_servers_answer_and_never_replays_a_sent_request() {
        use std::os::unix::net::UnixListener;

        for answer in [Some(Ok(Response::Ok)), Some(Err("workspace owns this file".to_string())), None] {
            let tag = match &answer {
                Some(Ok(_)) => "ok",
                Some(Err(_)) => "refused",
                None => "lost",
            };
            let name = format!("batch-{tag}-{}", std::process::id());
            let session = Session { explicit: Some(name.clone()), name };
            let dir = std::env::temp_dir().join(format!("kraal-{}", session.name));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("copy.kdl");
            let original = "layout { pane command=\"true\"; }";
            let replacement = "layout { pane command=\"false\"; }";
            std::fs::write(&path, original).unwrap();
            let expected = paths::FileRevision::read(&path).unwrap();
            let held = paths::SessionLock::acquire(&session.name).unwrap().unwrap();
            let socket = session.socket();
            let listener = UnixListener::bind(&socket).unwrap();
            std::thread::scope(|scope| {
                let server = scope.spawn(|| {
                    let (mut stream, _) = listener.accept().unwrap();
                    let (kind, bytes) = protocol::read_frame(&mut stream).unwrap().unwrap();
                    assert_eq!(kind, protocol::JSON);
                    let protocol::ClientMsg::Request { request, .. } = serde_json::from_slice(&bytes).unwrap() else {
                        panic!("not a request");
                    };
                    let Request::PublishWorkspaceFile { path: sent_path, source, expected: sent_expected } = request
                    else {
                        panic!("not a publication");
                    };
                    assert_eq!(sent_path, path);
                    assert_eq!(source, replacement);
                    assert_eq!(sent_expected, expected);
                    drop(listener);
                    std::fs::remove_file(&socket).unwrap();
                    drop(held);
                    if let Some(result) = answer.clone() {
                        protocol::write_json(&mut stream, &protocol::ServerMsg::Response { result }).unwrap();
                    }
                });
                let result = publish_import(&session, path.clone(), replacement.into(), expected.clone());
                match &answer {
                    Some(Ok(_)) => result.unwrap(),
                    Some(Err(why)) => assert_eq!(result.unwrap_err().to_string(), *why),
                    None => assert!(result.is_err()),
                }
                server.join().unwrap();
            });
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            assert!(!path.with_extension("kdl.bak").exists());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
