//! Panes from the command line: run, preview, send input, read, wait, follow events.

use std::path::PathBuf;

use anyhow::{Result, bail};

use super::args::{PlaceArgs, RunArgs};
use super::{Session, pane_ref};
use crate::protocol::{PaneInfo, Request, Response};
use crate::{client, paths, server};

/// `kraal panes` and `kraal agents`: the agents of the servers' sessions watched too, after
/// this session's own (an older server, still running, lists its own only).
pub(super) fn list(s: &Session, json: bool, agents: bool) -> Result<()> {
    let answer = if agents {
        s.request(Request::ListAgents).or_else(|_| s.request(Request::ListPanes))
    } else {
        s.request(Request::ListPanes)
    };
    let Response::Panes(list) = answer? else { bail!("unexpected answer") };
    let list: Vec<PaneInfo> = list.into_iter().filter(|p| !agents || p.agent.is_some()).collect();
    print_panes(&list, json, agents)
}

/// `kraal agents --follow`: this session's own agents as one JSON line (what `--json` lists of
/// them), then again each time that changes: at once for what events say (a state), within two
/// seconds otherwise (an agent that quit). It ends when what reads it goes away (another kraal's
/// ssh, listing them). The agents of servers this session watches are not in it: another kraal
/// watching this one would show them as this session's.
pub(super) fn follow_agents(s: &Session) -> Result<()> {
    use std::io::Write;
    let socket = s.socket();
    let (wake, woken) = std::sync::mpsc::channel::<()>();
    let mut events: Option<std::thread::JoinHandle<()>> = None;
    let mut last: Option<String> = None;
    loop {
        if output_closed(1) {
            return Ok(());
        }
        let agents: Vec<PaneInfo> = match s.request(Request::ListPanes) {
            Ok(Response::Panes(list)) => list.into_iter().filter(|p| p.agent.is_some()).collect(),
            Ok(_) => bail!("unexpected answer"),
            // Not running, yet or any more: no agents until it is.
            Err(_) if std::os::unix::net::UnixStream::connect(&socket).is_err() => Vec::new(),
            Err(e) => return Err(e),
        };
        // Events only wake this up; a subscription that ended is made again at the next look.
        if events.as_ref().is_none_or(|h| h.is_finished()) {
            let (socket, wake) = (socket.clone(), wake.clone());
            events = Some(std::thread::spawn(move || {
                let filters = vec!["agent.*".to_string(), "pane.*".to_string()];
                let _ = client::subscribe(&socket, filters, None, || {}, |_| wake.send(()).is_ok());
            }));
        }
        let line = serde_json::to_string(&agents)?;
        if last.as_ref() != Some(&line) {
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                return Ok(());
            }
            last = Some(line);
        }
        if woken.recv_timeout(std::time::Duration::from_secs(2)).is_ok() {
            // A burst of events is one look.
            std::thread::sleep(std::time::Duration::from_millis(100));
            while woken.try_recv().is_ok() {}
        }
    }
}

/// Whether nothing reads `fd` any more (the other end of its pipe or socket is closed), without
/// writing to it. Asked as a question about writing: macOS answers only that one.
fn output_closed(fd: std::os::fd::RawFd) -> bool {
    let mut p = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
    // SAFETY: one valid pollfd, owned by this frame; a timeout of 0 never blocks.
    let ready = unsafe { libc::poll(&mut p, 1, 0) };
    ready > 0 && p.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
}

fn print_panes(list: &[PaneInfo], json: bool, agents: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
        return Ok(());
    }
    for p in list {
        let name = p.name.clone().unwrap_or_else(|| {
            if p.title.is_empty() {
                p.command.first().cloned().unwrap_or_else(|| "shell".into())
            } else {
                p.title.clone()
            }
        });
        let mark = if p.focused { "*" } else { " " };
        let extra = match (&p.agent, p.exit_code) {
            _ if p.suspended => "not started (enter runs it)".to_string(),
            (_, Some(code)) => format!("exited {code}"),
            (Some(a), _) => format!("{} {a} {}", p.state.icon(), p.state.label()),
            (None, _) => p.cwd.as_ref().map(|c| paths::shorten_home(c)).unwrap_or_default(),
        };
        let extra = if p.production.is_some() { format!("PROD {extra}") } else { extra };
        if agents {
            // An agent of a server's session: its pane's id there, after the server's name.
            let workspace = match (&p.session, &p.server) {
                (Some(_), Some(server)) => format!("{server} › {}", p.workspace),
                _ => p.workspace.clone(),
            };
            println!("{:>4}{mark} {:<14} {:<24} {extra}", p.id, workspace, name);
        } else {
            println!("{:>4}{mark} {:<14} {:<12} {:<24} {extra}", p.id, p.workspace, p.tab, name);
        }
    }
    Ok(())
}

pub(super) fn run(s: &Session, args: RunArgs) -> Result<()> {
    let cwd = args.cwd.map(std::path::absolute).transpose()?.or_else(|| std::env::current_dir().ok());
    let r = s.request(Request::NewPane {
        target: args.place.target.clone().or_else(|| s.own_pane()),
        command: args.command,
        cwd,
        name: args.place.name.clone(),
        placement: args.place.placement(),
        preview: None,
        focus: !args.place.no_focus,
        close_on_exit: args.close_on_exit,
    })?;
    if let Response::Pane(p) = r {
        println!("{}", p.id);
    }
    Ok(())
}

pub(super) fn preview(s: &Session, path: PathBuf, place: PlaceArgs) -> Result<()> {
    let path = std::path::absolute(&path)?;
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let r = s.request(Request::NewPane {
        target: place.target.clone().or_else(|| s.own_pane()),
        command: Vec::new(),
        cwd: path.parent().map(PathBuf::from),
        name: place.name.clone(),
        placement: place.placement(),
        preview: Some(path),
        focus: !place.no_focus,
        close_on_exit: false,
    })?;
    if let Response::Pane(p) = r {
        println!("{}", p.id);
    }
    Ok(())
}

pub(super) fn send(s: &Session, pane: Option<String>, enter: bool, production: bool, text: Vec<String>) -> Result<()> {
    let mut t = text.join(" ");
    if enter {
        t.push('\r');
    }
    s.request(Request::SendText { pane: pane_ref(s, pane)?, text: t, production })?;
    Ok(())
}

pub(super) fn read(s: &Session, pane: Option<String>, lines: Option<usize>) -> Result<()> {
    if let Response::Text(t) = s.request(Request::ReadPane { pane: pane_ref(s, pane)?, lines })? {
        print!("{t}");
    }
    Ok(())
}

pub(super) fn wait(s: &Session, pane: Option<String>, until: Vec<String>, timeout: Option<f64>) -> Result<()> {
    let timeout_ms = timeout.map(|t| (t * 1000.0) as u64);
    if let Response::Pane(p) = s.request(Request::Wait { pane: pane_ref(s, pane)?, until, timeout_ms })? {
        println!("{}", p.state.label());
    }
    Ok(())
}

/// `kraal events`: one JSON line per event, as they happen (or replayed from `since`).
pub(super) fn events(s: &Session, types: Vec<String>, list: bool, since: Option<u64>, boot: Option<u64>) -> Result<()> {
    if list {
        for t in server::events::TYPES {
            println!("{t}");
        }
        return Ok(());
    }
    client::subscribe(
        &s.socket(),
        types,
        since.map(|since| (since, boot)),
        || {},
        |event| {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{event}").and_then(|_| out.flush()).is_ok()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_output_is_seen_without_writing() {
        use std::os::fd::AsRawFd;
        // A program another test starts at that moment may hold the reading end until it runs.
        let seen_closed = |fd| {
            let start = std::time::Instant::now();
            while !output_closed(fd) {
                if start.elapsed() > std::time::Duration::from_secs(5) {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            true
        };
        // A pipe, as sshd gives a command's output, and a socket, as some systems do.
        let (reader, writer) = std::io::pipe().unwrap();
        assert!(!output_closed(writer.as_raw_fd()));
        drop(reader);
        assert!(seen_closed(writer.as_raw_fd()));
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!output_closed(writer.as_raw_fd()));
        drop(reader);
        assert!(seen_closed(writer.as_raw_fd()));
    }
}
