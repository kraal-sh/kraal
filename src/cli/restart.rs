//! `kraal restart`: a session's server moves to the kraal installed now. Live when it can (see
//! `server::upgrade`): nothing stops. Otherwise the server stops and the next one starts from the
//! kraal installed now, bringing back what a restart keeps (see `persist`). `kraal update` offers
//! it for the sessions an update leaves on an older kraal.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::Session;
use crate::protocol::{self, Request, Response};
use crate::{client, paths, persist, process};

/// Which restart `kraal restart` does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum How {
    /// Live when it can, else not.
    Either,
    /// Live, or none.
    Live,
    /// The server stops and the next one brings the session back.
    Cold,
}

/// `kraal restart`: live when it can, asked first (nothing stops); else say what stops, ask,
/// then restart. Inside one of the session's panes, which a restart that is not live closes, a
/// helper of its own does that one.
pub(super) fn restart(s: &Session, yes: bool, how: How) -> Result<()> {
    let session = &s.name;
    let socket = paths::socket_path(session);
    if std::os::unix::net::UnixStream::connect(&socket).is_err() {
        bail!("session {session} is not running");
    }
    let running = Running::at(session.clone(), socket.clone());
    if how != How::Cold {
        match running.live(None) {
            Ok(()) => {
                println!(
                    "Session {session} runs {}; it restarts live on kraal {}: every program in its panes keeps running.",
                    running.runs(),
                    protocol::KRAAL_VERSION
                );
                if !yes && ask(&format!("Restart session {session}?"), true)? == Some(false) {
                    bail!("cancelled");
                }
                match running.restart_live() {
                    Ok((now, not_kept)) => {
                        println!("restarted session {session} live: {} → kraal {now}", running.runs());
                        if !not_kept.is_empty() {
                            // Their programs run on all the same: none was stopped.
                            eprintln!("Not taken over as it was (its program runs on):");
                            for line in &not_kept {
                                eprintln!("  {line}");
                            }
                        }
                        return Ok(());
                    }
                    Err(Live::Refused(why)) if how == How::Either => println!("It did not restart live: {why}."),
                    Err(Live::Refused(why)) => bail!("session {session} was not restarted: {why}"),
                    Err(Live::Failed(e)) => return Err(e),
                }
            }
            Err(why) if how == How::Live => bail!("session {session} cannot restart live: {why}"),
            Err(why) => println!("Session {session} cannot restart live: {why}."),
        }
    }
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
        let helper = client::restart_detached(&client::installed_exe()?, session, how == How::Cold)?;
        client::restart_under_way(helper, session, Duration::from_secs(1)).map_err(anyhow::Error::msg)?;
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
    let mut live = Vec::new();
    for r in &older {
        // The server tries the kraal just installed.
        match r.live(Some(exe)) {
            Ok(()) => {
                println!("Session {} still runs {}; it restarts live: nothing stops.", r.name, r.runs());
                live.push(true);
            }
            Err(why) => {
                let stops = r.stops();
                println!("Session {} still runs {}.", r.name, r.runs());
                println!("It cannot restart live: {why}.");
                print_stops(&stops);
                anything_stops |= !stops.is_empty();
                live.push(false);
            }
        }
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
    // The session this runs in goes last: a restart that is not live ends this command's pane.
    let here = std::env::var("KRAAL_SESSION").ok();
    let (last, first): (Vec<_>, Vec<_>) =
        older.into_iter().zip(live).partition(|(r, _)| here.as_deref() == Some(r.name.as_str()));
    let mut failed = 0;
    for (r, live) in first.into_iter().chain(last) {
        // The new kraal restarts it: this one may not know how the new one does. Said live, it
        // is live or not at all.
        let mut args = vec!["-s", &r.name, "restart", "--yes"];
        if live {
            args.push("--live");
        }
        let status = std::process::Command::new(exe)
            .args(args)
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

    /// Whether it can restart live now, on `binary` (else on this kraal's); why not otherwise.
    fn live(&self, binary: Option<&Path>) -> Result<(), String> {
        if self.server.is_none() {
            return Err("its server does not answer".into());
        }
        let binary = match binary {
            Some(b) => b.to_path_buf(),
            None => client::installed_exe().map_err(|e| format!("{e:#}"))?,
        };
        client::upgrade(&self.socket, true, Some(binary))
    }

    /// Restart it live on this kraal: the release it runs then, and what it could not take
    /// over as it was.
    fn restart_live(&self) -> Result<(String, Vec<String>), Live> {
        let exe = client::installed_exe().map_err(Live::Failed)?;
        let identity = client::identify(&self.socket).map_err(Live::Failed)?;
        client::upgrade(&self.socket, false, Some(exe)).map_err(Live::Refused)?;
        // It answers again once the new kraal has taken everything over: the same process, and
        // the same session (a server that could not take over starts afresh in that process),
        // one live restart further, which says what it took over.
        let before = identity.live.as_ref().map_or(0, |l| l.count);
        let start = Instant::now();
        loop {
            if let Ok(after) = client::identify(&self.socket) {
                if after.pid != identity.pid || after.boot != identity.boot {
                    // The same process that could not read what it took over keeps the
                    // terminals it was handed.
                    let kept = after.pid == identity.pid && after.live.is_some_and(|l| !l.not_kept.is_empty());
                    let what = match kept {
                        true => "the programs in its panes run on, in workspace recovered",
                        false => "its programs stopped",
                    };
                    return Err(Live::Failed(anyhow!(
                        "session {} restarted, but not live: {what} (see {})",
                        self.name,
                        paths::log_path(&self.name).display()
                    )));
                }
                let Some(live) = after.live.filter(|l| l.count == before + 1) else {
                    return Err(Live::Failed(anyhow!(
                        "session {} answers, but does not say it took over (see {})",
                        self.name,
                        paths::log_path(&self.name).display()
                    )));
                };
                return Ok((after.version, live.not_kept));
            }
            if start.elapsed() > Duration::from_secs(30) {
                return Err(Live::Failed(anyhow!(
                    "session {} did not answer again; `kraal` starts it with its layout",
                    self.name
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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

/// Why a live restart did not happen.
enum Live {
    /// The server said no: it went on as before.
    Refused(String),
    /// Something went wrong past that point.
    Failed(anyhow::Error),
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
