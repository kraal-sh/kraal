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
        || paths::session_state_path(session).exists();
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
    let state = remove_saved(&p)?;
    let scrollback = crate::persist::forget_scrollback(session)?;
    let buffers_path = paths::buffers_path(session);
    let buffers = remove_saved(&buffers_path)?;
    if state {
        println!("forgot {}", p.display());
    }
    if scrollback {
        println!("forgot {}", paths::scrollback_dir(session).display());
    }
    if buffers {
        println!("forgot {}", buffers_path.display());
    }
    if !state && !scrollback && !buffers {
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

/// `kraal workspaces`: the open ones with their agents, or the definitions when no server runs.
pub(super) fn workspaces(s: &Session, json: bool) -> Result<()> {
    let r = match s.request(Request::ListWorkspaces) {
        Ok(r) => r,
        Err(_) => return list_definitions(json),
    };
    let Response::Workspaces(list) = r else { bail!("unexpected answer") };
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    for w in list {
        let state = if w.open { "open" } else { "-" };
        let agents = if w.blocked + w.done + w.working > 0 {
            format!("  ▲{} ✓{} ●{}", w.blocked, w.done, w.working)
        } else {
            String::new()
        };
        println!("{:<20} {:<5} {:>3} tabs {:>4} panes{agents}", w.name, state, w.tabs, w.panes);
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
        println!("saved {}", p.display());
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
    if let Ok(rd) = std::fs::read_dir(paths::state_dir().join("sessions")) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json")
                && let Some(stem) = p.file_stem()
            {
                names.entry(stem.to_string_lossy().into()).or_insert_with(|| "saved".into());
            }
        }
    }
    if names.is_empty() {
        println!("no sessions");
    }
    for (n, s) in names {
        println!("{n:<20} {s}");
    }
    Ok(())
}

fn list_definitions(json: bool) -> Result<()> {
    let cfg = config::Config::load()?;
    let defs: Vec<_> = cfg.workspace_dirs().iter().flat_map(|d| workspace::load_dir(d)).collect();
    if json {
        let v: Vec<_> = defs
            .iter()
            .filter_map(|(p, d)| d.as_ref().ok().map(|d| serde_json::json!({"name": d.name, "open": false, "definition": p, "tabs": d.tabs.len(), "panes": d.panes().len()})))
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for (p, d) in defs {
        match d {
            Ok(d) => println!("{:<20} {:<5} {:>3} tabs {:>4} panes", d.name, "-", d.tabs.len(), d.panes().len()),
            Err(e) => println!("{:<20} error: {e:#}", p.display()),
        }
    }
    Ok(())
}

pub(super) fn import_zellij(from: Option<PathBuf>, force: bool) -> Result<()> {
    let from = from.unwrap_or_else(|| paths::expand_tilde("~/.config/zellij/layouts"));
    let to = paths::workspaces_dir();
    std::fs::create_dir_all(&to)?;
    let mut n = 0;
    for (path, def) in workspace::load_dir(&from) {
        let name = path.file_name().unwrap_or_default();
        match def {
            Err(e) => eprintln!("skipped {}: {e:#}", path.display()),
            Ok(def) => {
                let dest = to.join(name);
                if dest.exists() && !force {
                    eprintln!("skipped {} ({} exists; --force to overwrite)", path.display(), dest.display());
                    continue;
                }
                for w in &def.warnings {
                    eprintln!("{}: {w}", path.display());
                }
                std::fs::copy(&path, &dest)?;
                println!(
                    "{} → {} ({} tabs, {} panes)",
                    path.display(),
                    paths::shorten_home(&dest),
                    def.tabs.len(),
                    def.panes().len()
                );
                n += 1;
            }
        }
    }
    println!("imported {n} workspace{}", if n == 1 { "" } else { "s" });
    Ok(())
}
