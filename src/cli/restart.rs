//! `kraal restart`: a session's server stops and the next one starts from the kraal installed
//! now, bringing back what a restart keeps (see `persist`). `kraal update` offers it for the
//! sessions an update leaves on an older kraal.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Session;
use crate::protocol::{self, Request, Response};
use crate::{client, paths, persist, process};

/// `kraal restart`: say what stops, ask, then restart. Inside one of the session's panes, which
/// the restart closes, a helper of its own does it.
pub(super) fn restart(s: &Session, yes: bool) -> Result<()> {
    let session = &s.name;
    let socket = paths::socket_path(session);
    if std::os::unix::net::UnixStream::connect(&socket).is_err() {
        bail!("session {session} is not running");
    }
    let running = Running::at(session.clone(), socket.clone());
    let stops = running.stops();
    println!(
        "Session {session} runs {}; it restarts on kraal {}, with its layout, scrollback, SSH shells and agents.",
        running.runs(),
        protocol::KRAAL_VERSION
    );
    print_stops(&stops);
    if !yes {
        match ask(&format!("Restart session {session}?"), stops.is_empty())? {
            Some(true) => {}
            Some(false) => bail!("cancelled"),
            None if stops.is_empty() => {}
            None => bail!("programs would stop: add --yes to restart anyway"),
        }
    }
    if inside(session) {
        client::restart_detached(&client::installed_exe()?, session)?;
        println!("restarting session {session}…");
        return Ok(());
    }
    let saved = client::restart_server(session)?;
    let now = client::ping(&socket).map(|(_, v)| v).unwrap_or_else(|_| protocol::KRAAL_VERSION.into());
    match saved {
        client::Saved::No(why) => bail!("restarted session {session} on kraal {now}, but {why}"),
        _ => println!("restarted session {session}: {} → kraal {now}", running.runs()),
    }
    Ok(())
}

/// After an update to `installed` (the binary at `exe`): say which sessions still run an older
/// kraal and what restarting them stops, then restart them as `choice` says, or as asked
/// (no by default when programs would stop, and without a terminal to ask on).
pub(super) fn after_update(exe: &Path, installed: &str, choice: Option<bool>) -> Result<()> {
    let older: Vec<Running> = super::session::live_sockets()
        .into_iter()
        .map(|(name, socket)| Running::at(name, socket))
        .filter(|r| r.server.as_ref().is_none_or(|(_, v)| protocol::newer(installed, v)))
        .collect();
    if older.is_empty() {
        return Ok(());
    }
    let mut anything_stops = false;
    for r in &older {
        let stops = r.stops();
        println!("Session {} still runs {}.", r.name, r.runs());
        print_stops(&stops);
        anything_stops |= !stops.is_empty();
    }
    let them = if older.len() == 1 { "it" } else { "them" };
    let go = match choice {
        Some(go) => go,
        None => ask(&format!("Restart {them} on kraal {installed} now?"), !anything_stops)?.unwrap_or(false),
    };
    if !go {
        for r in &older {
            println!("Session {} keeps {} until: kraal -s {} restart", r.name, r.runs(), r.name);
        }
        return Ok(());
    }
    // The session this runs in goes last: its restart ends this command's pane.
    let here = std::env::var("KRAAL_SESSION").ok();
    let (last, first): (Vec<Running>, Vec<Running>) =
        older.into_iter().partition(|r| here.as_deref() == Some(r.name.as_str()));
    let mut failed = 0;
    for r in first.into_iter().chain(last) {
        // The new kraal restarts it: this one may not know how the new one does.
        let status = std::process::Command::new(exe)
            .args(["-s", &r.name, "restart", "--yes"])
            .status()
            .with_context(|| format!("starting {}", exe.display()))?;
        if !status.success() {
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} session{} could not be restarted", if failed == 1 { "" } else { "s" });
    }
    Ok(())
}

/// A session whose server runs.
struct Running {
    name: String,
    socket: PathBuf,
    /// Its pid and kraal release; `None` when it does not answer (another protocol).
    server: Option<(u32, String)>,
}

impl Running {
    fn at(name: String, socket: PathBuf) -> Running {
        let server = client::ping(&socket).ok();
        Running { name, socket, server }
    }

    fn runs(&self) -> String {
        match &self.server {
            Some((_, version)) => format!("kraal {version}"),
            None => "a kraal this one cannot talk to".into(),
        }
    }

    /// What restarting it stops, a line per pane. The pane this command runs in is left out:
    /// it comes back as it is.
    fn stops(&self) -> Vec<String> {
        let unknown = || vec!["whatever runs in its panes (its server does not say)".to_string()];
        let Some((pid, _)) = &self.server else { return unknown() };
        let Ok(Response::Panes(panes)) = client::request(&self.socket, Request::ListPanes) else { return unknown() };
        let own = own_pane(&self.name);
        let programs = pane_programs(*pid, &self.socket);
        panes
            .iter()
            .filter(|p| Some(p.id) != own)
            .filter_map(|p| {
                let what = persist::stops_on_restart(p, programs.get(&p.id).map(Vec::as_slice))?;
                Some(persist::stopping_line(p, &what))
            })
            .collect()
    }
}

/// What runs in each pane of the server `server` (listening on `socket`), by pane id, when it is
/// not a shell waiting at its prompt: the program in the foreground of the pane's terminal, or
/// the pane's own program. Read from the processes, which every kraal version starts the same
/// way, so that a server older than this question is asked nothing it cannot answer.
fn pane_programs(server: u32, socket: &Path) -> HashMap<u32, Vec<String>> {
    use std::os::unix::fs::MetadataExt;
    let Ok(listening) = std::fs::metadata(socket) else { return HashMap::new() };
    let ours =
        |path: &str| std::fs::metadata(path).is_ok_and(|m| m.dev() == listening.dev() && m.ino() == listening.ino());
    let mut out = HashMap::new();
    for pid in process::children(server) {
        // Its other children (hooks, tunnels) may have a pane's variables from wherever the
        // server was started: a pane has this session's socket.
        let env = process::environ(pid, &["KRAAL_PANE", "KRAAL_SOCKET"]);
        if !env.get("KRAAL_SOCKET").is_some_and(|s| ours(s)) {
            continue;
        }
        let Some(id) = env.get("KRAAL_PANE").and_then(|v| v.parse::<u32>().ok()) else { continue };
        let program = match process::foreground_group(pid).filter(|g| *g != pid) {
            Some(group) => process::argv(group),
            None => process::argv(pid).filter(|argv| !crate::pane::is_shell(&argv[0])),
        };
        if let Some(argv) = program {
            out.insert(id, argv);
        }
    }
    out
}

fn print_stops(stops: &[String]) {
    if stops.is_empty() {
        return;
    }
    println!("These programs stop:");
    for line in stops {
        println!("  {line}");
    }
}

/// Whether this command runs in a pane of `session`.
fn inside(session: &str) -> bool {
    std::env::var("KRAAL_SESSION").is_ok_and(|s| s == session) && std::env::var_os("KRAAL_PANE").is_some()
}

/// The pane this command runs in, when it is one of `session`'s.
fn own_pane(session: &str) -> Option<u32> {
    inside(session).then(|| std::env::var("KRAAL_PANE").ok()?.parse().ok()).flatten()
}

/// Ask `question` on the terminal; Enter answers `default`. `None` without a terminal: there
/// is no one to ask.
fn ask(question: &str, default: bool) -> Result<Option<bool>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    eprint!("{question} {} ", if default { "[Y/n]" } else { "[y/N]" });
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(Some(match line.trim() {
        "" => default,
        answer => matches!(answer, "y" | "Y" | "yes"),
    }))
}
