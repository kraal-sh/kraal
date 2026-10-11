//! Sessions and workspaces: attach, list, open, save, stop.

use std::path::PathBuf;

use anyhow::{Result, bail};

use super::Session;
use crate::protocol::{Request, Response};
use crate::{client, config, paths, workspace};

pub(super) fn attach(session: &str, workspace: Option<String>) -> Result<()> {
    let cfg = config::Config::load().unwrap_or_default();
    client::attach(client::Target::Local { session: session.to_string() }, workspace, cfg.mouse, || {})
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
    if std::os::unix::net::UnixStream::connect(paths::socket_path(session)).is_ok() {
        bail!("session {session} is running; `kraal kill` it first");
    }
    let p = paths::session_state_path(session);
    let state = match std::fs::remove_file(&p) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    let scrollback = crate::persist::forget_scrollback(session)?;
    if state {
        println!("forgot {}", p.display());
    }
    if scrollback {
        println!("forgot {}", paths::scrollback_dir(session).display());
    }
    if !state && !scrollback {
        println!("nothing saved for {session}");
    }
    Ok(())
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
    client::ensure_server(&s.name)?;
    s.request(Request::OpenWorkspace { name: name.clone() })?;
    println!("workspace {name} is open; `kraal attach {name}` to see it");
    Ok(())
}

pub(super) fn save(s: &Session, name: Option<String>, workspace: Option<String>) -> Result<()> {
    if let Response::Path(p) = s.request(Request::SaveWorkspace { workspace, name })? {
        println!("saved {}", p.display());
    }
    Ok(())
}

pub(super) fn list_sessions() -> Result<()> {
    let mut names = std::collections::BTreeMap::<String, &str>::new();
    // Sessions an older kraal started may still listen where it put them.
    let dirs = std::iter::once(paths::runtime_dir()).chain(paths::legacy_runtime_dir());
    for rd in dirs.filter_map(|d| std::fs::read_dir(d).ok()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "sock")
                && let Some(stem) = p.file_stem()
            {
                let alive = std::os::unix::net::UnixStream::connect(&p).is_ok();
                // A long session name has a shortened socket name: the saved session says which.
                let stem = stem.to_string_lossy();
                if alive {
                    names.insert(paths::session_of_socket(&stem).unwrap_or_else(|| stem.into_owned()), "running");
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(paths::state_dir().join("sessions")) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json")
                && let Some(stem) = p.file_stem()
            {
                names.entry(stem.to_string_lossy().into()).or_insert("saved");
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
