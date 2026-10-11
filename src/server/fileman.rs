//! The two-panel file manager pane: opening it, and acting on what it asks for (copy, move,
//! delete, preview, shell, choosing a server for a panel).

use std::sync::Arc;

use anyhow::Result;

use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Event, FloatRect, Server};
use crate::files::{FileKey, FilesAction, FilesPane, Location, Remote, Side};
use crate::input::{Key, KeyCode};
use crate::layout::PaneId;
use crate::pane::{Content, PaneSpec};
use crate::paths;
use crate::protocol::Placement;

impl Server {
    /// `/path`, `~/x` or `server:path` as a panel location.
    pub(super) fn files_location(&self, spec: &str) -> Location {
        if let Some((head, path)) = crate::transfer::split_remote(spec)
            && let Some(s) = self.known_servers.iter().find(|k| k.server.name == head).map(|k| k.server.clone())
        {
            let mut ssh_args = self.ssh_options(&s);
            ssh_args.push(s.destination());
            let path = if path.is_empty() { "~".to_string() } else { path.to_string() };
            return Location::Remote(Remote { server: s.name.clone(), ssh_args, path });
        }
        Location::Local(paths::expand_tilde(spec))
    }

    pub(super) fn file_manager(&self, left: &str, right: &str) -> FilesPane {
        let tx = self.tx.clone();
        // Called from the listing's own thread: waiting for room in the queue costs nothing there,
        // while a wake-up dropped would leave the panel loading for good.
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.send(Event::Wake);
        });
        FilesPane::new(self.files_location(left), self.files_location(right), wake)
    }

    /// Open a file manager as a large floating pane.
    pub fn open_files(&mut self, ws: u32, left: String, right: String) -> Result<PaneId> {
        let spec = PaneSpec { files: Some([left, right]), ..Default::default() };
        let id = self.spawn_pane_in(ws, None, spec, Placement::Floating, None)?;
        if let Some((wi, ti)) = self.workspace_of(id)
            && let Some(f) = self.workspaces[wi].tabs[ti].floating.iter_mut().find(|f| f.pane == id)
        {
            f.rect = FloatRect { x: 0.03, y: 0.04, w: 0.94, h: 0.92 };
        }
        self.relayout();
        Ok(id)
    }

    /// Default panels for a client: the focused pane's directory, and its server if it has one.
    pub(super) fn files_defaults(&self, client: ClientId) -> (String, String) {
        let focus = self.client_focus(client).and_then(|p| self.panes.get(&p));
        let local = focus
            .filter(|p| p.ssh.is_none())
            .and_then(|p| p.current_cwd())
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default());
        let right = match focus.and_then(|p| p.ssh.as_ref()) {
            Some(s) if self.known_servers.iter().any(|k| k.server.name == s.name) => format!("{}:~", s.name),
            _ => dirs::home_dir().unwrap_or_default().display().to_string(),
        };
        (local.display().to_string(), right)
    }

    /// The size this client sees the file manager at: a smaller terminal shows less of it, and
    /// clicks, pages and the wheel are counted in what it shows. As the pane is drawn: its frame
    /// clipped to the client's screen, then the content inside it.
    pub(super) fn files_view(&self, client: ClientId, pane: PaneId) -> Option<(u16, u16)> {
        let c = self.clients.get(&client)?;
        let wi = self.ws_index(c.workspace)?;
        let (_, outer, floating) =
            self.tab_rects(self.workspaces[wi].tab(), self.layout_area).into_iter().find(|(p, _, _)| *p == pane)?;
        let (_, inner) = self.placed(outer, floating, ratatui::layout::Rect::new(0, 0, c.cols, c.rows));
        let p = self.panes.get(&pane)?;
        Some((inner.width.min(p.cols), inner.height.min(p.rows)))
    }

    /// A key typed in a file manager pane.
    pub(super) fn files_key(&mut self, client: ClientId, pane: PaneId, k: &Key) -> Mode {
        let fk = match k.code {
            _ if k.mods.ctrl || k.mods.alt => return Mode::Normal,
            KeyCode::Up => FileKey::Up,
            KeyCode::Down => FileKey::Down,
            KeyCode::PageUp => FileKey::PageUp,
            KeyCode::PageDown => FileKey::PageDown,
            KeyCode::Home => FileKey::Home,
            KeyCode::End => FileKey::End,
            KeyCode::Enter => FileKey::Enter,
            KeyCode::Backspace => FileKey::Backspace,
            KeyCode::Tab | KeyCode::BackTab => FileKey::Tab,
            KeyCode::Esc => FileKey::Esc,
            KeyCode::Delete => FileKey::Delete,
            KeyCode::Insert => FileKey::Insert,
            KeyCode::F(n) => FileKey::F(n),
            KeyCode::Char(c) => FileKey::Char(c),
            KeyCode::Left | KeyCode::Right => FileKey::Tab,
        };
        let view = self.files_view(client, pane);
        let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) else { return Mode::Normal };
        if let Some((w, h)) = view {
            f.set_view(client, w, h);
        }
        let action = f.key(fk);
        self.mark_dirty();
        self.files_action(client, pane, action)
    }

    /// Mouse click in a file manager pane, at pane-relative cell (col, row).
    pub(super) fn files_click(&mut self, client: ClientId, pane: PaneId, col: u16, row: u16, double: bool) -> Mode {
        let view = self.files_view(client, pane);
        let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) else { return Mode::Normal };
        if let Some((w, h)) = view {
            f.set_view(client, w, h);
        }
        let action = f.click(col, row, double);
        self.mark_dirty();
        self.files_action(client, pane, action)
    }

    pub(super) fn files_action(&mut self, client: ClientId, pane: PaneId, action: FilesAction) -> Mode {
        // The places a job acts on, as the CLI reads them; a local path that is not UTF-8 cannot be
        // passed exactly, and a server whose settings changed since its panel was listed would be
        // another machine than the one shown.
        let places: Vec<&Location> = match &action {
            FilesAction::Copy { sources, dest, .. } | FilesAction::Move { sources, dest, .. } => {
                sources.iter().chain([dest]).collect()
            }
            FilesAction::Delete(targets) => targets.iter().collect(),
            FilesAction::Shell(loc) => vec![loc],
            _ => Vec::new(),
        };
        if places.iter().any(|l| l.exact_display().is_none()) {
            self.toast(client, "a path here is not valid UTF-8: kraal cannot point at it safely; use a shell");
            return Mode::Normal;
        }
        if let Some(server) = self.stale_server(&places) {
            self.reconnect_panels(pane);
            self.toast(
                client,
                format!("{server} is not reached as when its panel was listed: check the panel and retry"),
            );
            return Mode::Normal;
        }
        let expect: Vec<String> = places
            .iter()
            .filter_map(|l| match l {
                Location::Remote(r) => Some(format!("--expect={}={}", r.server, crate::shell::join(&r.ssh_args))),
                Location::Local(_) => None,
            })
            .collect();
        let dir_arg = |l: &Location| {
            let shown = l.display();
            if shown.ends_with('/') { shown } else { format!("{shown}/") }
        };
        match action {
            FilesAction::None => Mode::Normal,
            FilesAction::Copy { sources, dest, replaces } => {
                // What the panel shows can be out of date: the copy itself checks again, and
                // replaces only what the user agreed to.
                let mut args = vec!["cp".to_string(), "--no-replace".to_string()];
                for name in &replaces {
                    args.extend(["--allow".to_string(), name.clone()]);
                }
                args.extend(expect);
                args.push("--".into());
                args.extend(sources.iter().map(|s| s.display()));
                args.push(dir_arg(&dest));
                if replaces.is_empty() {
                    self.start_job(client, "copy", args);
                    return Mode::Normal;
                }
                // Copying over files is a decision, not a side effect.
                // Every name, one per line: the question scrolls.
                let message = format!(
                    "Copy {} to {}?\nIt replaces what is there:\n  {}",
                    describe(&sources),
                    dest.display(),
                    replaces.join("\n  ")
                );
                Mode::confirm(message, ConfirmAction::Job { label: "copy and replace".into(), args })
            }
            FilesAction::Move { sources, dest, replaces } => {
                let mut args = vec!["cp".to_string(), "--move".to_string(), "--no-replace".to_string()];
                for name in &replaces {
                    args.extend(["--allow".to_string(), name.clone()]);
                }
                args.extend(expect);
                args.push("--".into());
                args.extend(sources.iter().map(|s| s.display()));
                args.push(dir_arg(&dest));
                let mut message = format!("Move {} to {}?", describe(&sources), dest.display());
                if !replaces.is_empty() {
                    message.push_str(&format!("\nIt replaces what is there:\n  {}", replaces.join("\n  ")));
                }
                Mode::confirm(message, ConfirmAction::Job { label: "move".into(), args })
            }
            FilesAction::Delete(targets) => {
                let mut args = vec!["rm".to_string()];
                args.extend(expect);
                args.push("--".into());
                args.extend(targets.iter().map(|s| s.display()));
                let list: Vec<String> = targets.iter().map(|t| t.display()).collect();
                let what = if list.len() == 1 { list[0].clone() } else { format!("these {} items", list.len()) };
                let mut message = format!("Delete {what} for good?");
                if list.len() > 1 {
                    message.push_str(&format!("\n  {}", list.join("\n  ")));
                }
                Mode::confirm(message, ConfirmAction::Job { label: "delete".into(), args })
            }
            FilesAction::Preview(path) => {
                let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return Mode::Normal };
                let spec = PaneSpec { preview: Some(path), ..Default::default() };
                if let Err(e) = self.spawn_pane_in(ws, Some(pane), spec, Placement::Floating, None) {
                    self.toast(client, format!("{e:#}"));
                }
                Mode::Normal
            }
            FilesAction::Shell(loc) => {
                let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return Mode::Normal };
                let result = match &loc {
                    Location::Local(p) => {
                        let spec = PaneSpec { cwd: Some(p.clone()), ..Default::default() };
                        self.spawn_pane_in(ws, None, spec, Placement::Tab, None)
                    }
                    Location::Remote(r) => match self.resolve_server(&r.server) {
                        Some(s) => {
                            let mut command = self.ssh_command(&s);
                            command.insert(1, "-t".into());
                            let cd = if r.path == "~" || r.path.is_empty() {
                                "cd".to_string()
                            } else {
                                format!("cd {}", crate::shell::quote(&r.path))
                            };
                            // A POSIX script, whatever the login shell there (fish, csh…).
                            let script = format!("{cd} && exec \"${{SHELL:-sh}}\" -l");
                            command.push(format!("sh -c {}", crate::shell::quote_always(&script)));
                            let spec = PaneSpec { command, server: Some(s.name.clone()), ..Default::default() };
                            self.spawn_pane_in(ws, None, spec, Placement::Tab, Some(s.name.clone()))
                        }
                        None => Err(anyhow::anyhow!("unknown server {}", r.server)),
                    },
                };
                if let Err(e) = result {
                    self.toast(client, format!("{e:#}"));
                }
                Mode::Normal
            }
            FilesAction::PickLocation(side) => Mode::Picker(self.location_picker(pane, side)),
            FilesAction::Refused(why) => {
                self.toast(client, why);
                Mode::Normal
            }
            FilesAction::Close => {
                self.close_pane(pane);
                Mode::Normal
            }
        }
    }

    fn location_picker(&self, pane: PaneId, side: Side) -> Picker {
        let mut items = vec![PickItem {
            label: "local".into(),
            detail: format!("this machine · {}", paths::shorten_home(&dirs::home_dir().unwrap_or_default())),
            action: PickAction::FilesSet { pane, side, target: "~".into() },
            state: None,
            dim: false,
        }];
        for k in &self.known_servers {
            items.push(PickItem {
                label: k.server.name.clone(),
                detail: format!("{} · {}", k.server.address(), k.server.groups.join(", ")),
                action: PickAction::FilesSet { pane, side, target: format!("{}:~", k.server.name) },
                state: None,
                dim: false,
            });
        }
        let which = if side == Side::Left { "left" } else { "right" };
        Picker::new(PickerKind::Pane, &format!("Show in the {which} panel"), items)
    }

    /// The ssh arguments that reach `server` now, destination last (what a panel lists with).
    fn panel_ssh_args(&self, server: &str) -> Option<Vec<String>> {
        let s = self.known_servers.iter().find(|k| k.server.name == server)?.server.clone();
        let mut args = self.ssh_options(&s);
        args.push(s.destination());
        Some(args)
    }

    /// A server among `places` that is not reached as its panel listed it any more.
    fn stale_server(&self, places: &[&Location]) -> Option<String> {
        places.iter().find_map(|l| match l {
            Location::Remote(r) if self.panel_ssh_args(&r.server).as_ref() != Some(&r.ssh_args) => {
                Some(r.server.clone())
            }
            _ => None,
        })
    }

    /// List the remote panels of a file manager again with their servers' current settings.
    fn reconnect_panels(&mut self, pane: PaneId) {
        for side in [Side::Left, Side::Right] {
            let Some(Content::Files(f)) = self.panes.get(&pane).map(|p| &p.content) else { return };
            let Location::Remote(r) = f.location(side) else { continue };
            // A server gone from the inventory stays as it was: its listing fails, and so would a job.
            if self.panel_ssh_args(&r.server).is_none_or(|now| now == r.ssh_args) {
                continue;
            }
            let loc = self.files_location(&format!("{}:{}", r.server, r.path));
            if let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) {
                f.set_location(side, loc);
            }
        }
        self.mark_dirty();
    }

    pub(super) fn files_set(&mut self, pane: PaneId, side: Side, target: &str) {
        let loc = self.files_location(target);
        if let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) {
            f.set_location(side, loc);
        }
        self.mark_persist();
        self.mark_dirty();
    }

    /// Reload every file manager (after a transfer finished).
    pub(super) fn refresh_file_managers(&mut self) {
        for p in self.panes.values_mut() {
            if let Content::Files(f) = &mut p.content {
                f.refresh();
            }
        }
    }

    /// Run `kraal <args>` in a floating pane that closes itself a few seconds after success.
    pub(super) fn start_job(&mut self, client: ClientId, label: &str, args: Vec<String>) {
        let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return };
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "kraal".into());
        let mut command = vec![exe];
        command.extend(args);
        let spec = PaneSpec { command, close_on_success: true, ..Default::default() };
        let previous = self.client_focus(client);
        match self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(format!("⇅ {label}"))) {
            Ok(id) => {
                // A small window in the bottom-right corner, on top, that does not take the focus.
                if let Some((wi, ti)) = self.workspace_of(id) {
                    let tab = &mut self.workspaces[wi].tabs[ti];
                    if let Some(f) = tab.floating.iter_mut().find(|f| f.pane == id) {
                        f.rect = FloatRect { x: 0.5, y: 0.58, w: 0.48, h: 0.4 };
                    }
                    if let Some(prev) = previous.filter(|p| tab.contains(*p)) {
                        tab.focus = Some(prev);
                        tab.recent.retain(|p| *p != prev);
                        tab.recent.insert(0, prev);
                    }
                }
                self.relayout();
            }
            Err(e) => self.toast(client, format!("{e:#}")),
        }
    }
}

/// What a file operation acts on: the place, or the items by name.
fn describe(locs: &[Location]) -> String {
    match locs {
        [one] => one.display(),
        many => {
            let shown: Vec<String> = many.iter().filter_map(Location::name).collect();
            format!("{} items ({})", many.len(), names(&shown))
        }
    }
}

/// Up to five names, then how many more.
fn names(list: &[String]) -> String {
    let mut out = list.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if list.len() > 5 {
        out.push_str(&format!(" and {} more", list.len() - 5));
    }
    out
}
