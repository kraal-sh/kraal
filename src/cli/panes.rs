//! Panes from the command line: run, preview, send input, read, wait, follow events.

use std::path::PathBuf;

use anyhow::{Result, bail};

use super::args::{PlaceArgs, RunArgs};
use super::{Session, pane_ref};
use crate::protocol::{PaneInfo, Request, Response};
use crate::{client, paths, server};

/// `kraal panes` and `kraal agents`.
pub(super) fn list(s: &Session, json: bool, agents: bool) -> Result<()> {
    let Response::Panes(list) = s.request(Request::ListPanes)? else { bail!("unexpected answer") };
    let list: Vec<PaneInfo> = list.into_iter().filter(|p| !agents || p.agent.is_some()).collect();
    print_panes(&list, json, agents)
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
            println!("{:>4}{mark} {:<14} {:<24} {extra}", p.id, p.workspace, name);
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
