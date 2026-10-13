//! Markdown runbooks: their shell blocks, sent one at a time to the pane they run in.

use std::collections::HashSet;

use anyhow::Result;

use super::{MdSession, first_line};
use crate::agent::AgentState;
use crate::input::encode_paste;
use crate::pane::PaneSpec;
use crate::paths;
use crate::protocol::Placement;
use crate::runbooks::{self, Body, Runbook, Source};
use crate::server::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use crate::server::{ClientId, Server};
use crate::text::numbered;

impl Server {
    pub(super) fn open_markdown(&mut self, client: ClientId, r: Runbook) -> Mode {
        let Body::Markdown { title, blocks } = r.body.clone() else { return Mode::Normal };
        if blocks.is_empty() {
            self.toast(client, "no shell blocks (```sh) in this runbook");
            return Mode::Normal;
        }
        let Some(target) = self.client_focus(client).filter(|p| self.panes.get(p).is_some_and(|p| p.is_terminal()))
        else {
            let key = self.runbooks_key();
            self.toast(client, format!("focus the terminal pane the steps should run in, then {key}"));
            return Mode::Normal;
        };
        let ws = self.clients.get(&client).map(|c| c.workspace).unwrap_or(0);
        let shown = self.panes.values().any(|p| p.spec.preview.as_deref() == Some(r.path.as_path()));
        if !shown {
            let spec = PaneSpec { preview: Some(r.path.clone()), ..Default::default() };
            if let Err(e) = self.spawn_pane_in(ws, Some(target), spec, Placement::Right, Some(r.name.clone())) {
                self.toast(client, format!("{e:#}"));
            }
            // Keep typing where the steps run.
            if let Some((wi, ti)) = self.workspace_of(target) {
                self.workspaces[wi].tabs[ti].set_focus(target);
            }
        }
        let title = if title.is_empty() { r.name.clone() } else { title };
        let session =
            MdSession { runbook: r, title, blocks, target, sent: HashSet::new(), approved: None, remote_ok: false };
        self.md_sessions.insert(client, session);
        Mode::Picker(self.md_picker(client, 0, false))
    }

    pub(super) fn md_picker(&self, client: ClientId, select: usize, back: bool) -> Picker {
        let s = &self.md_sessions[&client];
        let mut items: Vec<PickItem> = s
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let heading = if b.heading.is_empty() { first_line(&b.code) } else { b.heading.clone() };
                let detail = format!("{}{}", if s.sent.contains(&i) { "sent · " } else { "" }, first_line(&b.code));
                PickItem::new(format!("{}. {heading}", i + 1), detail, PickAction::RunBlock(i))
                    .with_state(s.sent.contains(&i).then_some(AgentState::Done))
            })
            .collect();
        if back {
            items.push(PickItem::new("‹ other runbooks", "", PickAction::RunbookList).dim());
        }
        let title = format!("{} · runs in {}", s.title, self.pane_label(s.target));
        let mut picker = Picker::new(PickerKind::RunbookSteps, &title, items);
        picker.selected = select.min(picker.filtered.len().saturating_sub(1));
        picker
    }

    /// Run block `index` in the session's pane: refused while a program other than the shell
    /// runs there (the block would go to it); asks first for repository runbooks not trusted
    /// yet, production panes, and panes on a server (kraal cannot see what runs there).
    pub(in crate::server) fn run_block(&mut self, client: ClientId, index: usize) -> Mode {
        self.with_fresh_md(client, index, move |server| server.ask_block(client, index))
    }

    fn ask_block(&mut self, client: ClientId, index: usize) -> Mode {
        let concerns = match self.block_check(client, index) {
            Ok(c) => c,
            Err(why) => {
                self.toast(client, why);
                return Mode::Normal;
            }
        };
        if concerns.is_empty() {
            return self.send_block(client, index);
        }
        let Some(s) = self.md_sessions.get(&client) else { return Mode::Normal };
        let shown = |code: &str| s.runbook.source.shown(code);
        let mut lines = vec![format!("Run step {} in {}?", index + 1, self.pane_label(s.target))];
        lines.extend(concerns);
        lines.extend(shown(&s.blocks[index].code).lines().map(|line| format!("  {line}")));
        // A yes approves the document, so every step it could then send is there to read.
        if s.unapproved() && s.blocks.len() > 1 {
            lines.push("Its other steps:".into());
            for (i, block) in s.blocks.iter().enumerate().filter(|(i, _)| *i != index) {
                lines.extend(numbered(i + 1, &shown(&block.code)));
            }
        }
        let production = self.pane_production(s.target);
        Mode::confirm(lines.join("\n"), ConfirmAction::RunBlock { index, production })
    }

    /// Read the session's runbook again (off the event loop) before going on with block `index`
    /// with `next`: a document edited since it was opened must not run its old blocks.
    fn with_fresh_md(
        &mut self,
        client: ClientId,
        index: usize,
        next: impl FnOnce(&mut Server) -> Mode + 'static,
    ) -> Mode {
        let Some(s) = self.md_sessions.get(&client) else { return Mode::Normal };
        let (path, source) = (s.runbook.path.clone(), s.runbook.source.clone());
        let title = format!("{} · reading it again", s.title);
        self.wait_for(
            client,
            PickerKind::RunbookSteps,
            &title,
            move || runbooks::load(&path, source),
            move |server, loaded| match server.refresh_md(client, index, loaded) {
                Some(mode) => mode,
                None => next(server),
            },
        )
    }

    /// Compare the runbook as just read (`loaded`) with the session's, and with what its
    /// preview shows. When the file changed, the session takes the new blocks (none sent) and the
    /// user chooses again from them. When a preview of it shows another version (it has not
    /// caught up yet), nothing runs: what runs is always what is read on screen. `None` when all
    /// agree.
    fn refresh_md(&mut self, client: ClientId, index: usize, loaded: Result<Runbook>) -> Option<Mode> {
        let s = self.md_sessions.get(&client)?;
        let fresh = match loaded {
            Ok(r) if r.content == s.runbook.content => {
                let shown_elsewhere = self.panes.values_mut().any(|p| {
                    let Some(pv) = p.preview_of(&s.runbook.path) else { return false };
                    let stale = pv.markdown_source().is_some_and(|shown| shown != s.runbook.content);
                    if stale {
                        pv.reload();
                    }
                    stale
                });
                if !shown_elsewhere {
                    return None;
                }
                let back = index.min(s.blocks.len() - 1);
                self.toast(
                    client,
                    "the preview shows another version of the runbook: wait for it to update, then choose again",
                );
                return Some(Mode::Picker(self.md_picker(client, back, false)));
            }
            Ok(r) => r,
            Err(e) => {
                self.toast(client, format!("{e:#}"));
                return Some(Mode::Normal);
            }
        };
        let Body::Markdown { title, blocks } = fresh.body.clone() else { return Some(Mode::Normal) };
        if blocks.is_empty() {
            self.md_sessions.remove(&client);
            self.toast(client, "the runbook changed and has no shell blocks any more");
            return Some(Mode::Normal);
        }
        let s = self.md_sessions.get_mut(&client)?;
        s.title = if title.is_empty() { s.runbook.name.clone() } else { title };
        s.blocks = blocks;
        s.sent.clear();
        s.runbook = Runbook { name: s.runbook.name.clone(), ..fresh };
        let last = s.blocks.len() - 1;
        self.toast(client, "the runbook changed since it was opened: these are its steps now, choose again");
        Some(Mode::Picker(self.md_picker(client, index.min(last), false)))
    }

    /// What stands in the way of sending block `index` now: `Err` refuses, the list asks.
    pub(super) fn block_check(&mut self, client: ClientId, index: usize) -> Result<Vec<String>, String> {
        let s = self.md_sessions.get(&client).ok_or("no runbook in progress")?;
        if index >= s.blocks.len() {
            return Err("no such step".into());
        }
        let target = s.target;
        let Some(pane) = self.panes.get(&target).filter(|p| p.exit_code.is_none() && p.is_terminal()) else {
            self.md_sessions.remove(&client);
            return Err(format!("the pane the steps ran in is gone; focus another one and {}", self.runbooks_key()));
        };
        let label = self.pane_label(target);
        if pane.ssh.is_none() && !pane.shell_waiting() {
            let what = pane
                .foreground_argv()
                .or_else(|| pane.pid().and_then(crate::process::argv))
                .and_then(|a| a.first().cloned())
                .unwrap_or_else(|| "a program".into());
            let what = what.rsplit('/').next().unwrap_or(&what).to_string();
            return Err(format!("{label} is running {what}, not a shell at its prompt: the step was not sent"));
        }
        let mut concerns = Vec::new();
        if let Source::Repo(root) = &s.runbook.source
            && s.unapproved()
        {
            let root = s.runbook.source.shown(&paths::shorten_home(root));
            concerns.push(format!(
                "From the repository {root}: new, or changed since you approved it. A yes approves it as it is \
                 now, with all its steps."
            ));
        }
        if let Some(why) = self.pane_production(target)
            && s.approved.as_ref() != Some(&why)
        {
            concerns.push(format!("{label} is production ({why})."));
        }
        if let Some(server) = &pane.ssh
            && !s.remote_ok
        {
            concerns.push(format!(
                "{label} is on {}: kraal cannot see whether a program waits there; the step is typed as is.",
                server.name
            ));
        }
        Ok(concerns)
    }

    pub(in crate::server) fn confirm_block(
        &mut self,
        client: ClientId,
        index: usize,
        production: Option<crate::server::contexts::Production>,
        yes: bool,
    ) -> Mode {
        if !yes {
            return Mode::Normal;
        }
        // The question showed the block as it was then: an edit since is chosen again.
        self.with_fresh_md(client, index, move |server| server.approve_block(client, index, production))
    }

    fn approve_block(
        &mut self,
        client: ClientId,
        index: usize,
        production: Option<crate::server::contexts::Production>,
    ) -> Mode {
        let Some(s) = self.md_sessions.get_mut(&client) else { return Mode::Normal };
        // What the question showed is what is approved: a pane that became production since is
        // asked about again by the check below.
        s.approved = production;
        s.remote_ok = true;
        if matches!(s.runbook.source, Source::Repo(_))
            && let Err(e) = runbooks::trust(&s.runbook)
        {
            self.warnings.push(format!("{e:#}"));
        }
        // The answer may come late: check again right before typing.
        match self.block_check(client, index) {
            Ok(concerns) if concerns.is_empty() => self.send_block(client, index),
            Ok(_) => self.ask_block(client, index),
            Err(why) => {
                self.toast(client, why);
                Mode::Normal
            }
        }
    }

    pub(super) fn send_block(&mut self, client: ClientId, index: usize) -> Mode {
        let Some(s) = self.md_sessions.get(&client) else { return Mode::Normal };
        let Some(code) = s.blocks.get(index).map(|b| b.code.clone()) else { return Mode::Normal };
        let (target, total) = (s.target, s.blocks.len());
        let Some(p) = self.panes.get_mut(&target) else { return Mode::Normal };
        let bytes = encode_paste(code.as_bytes(), p.input_modes());
        p.write_input(bytes);
        p.write_input(b"\r".to_vec());
        let Some(s) = self.md_sessions.get_mut(&client) else { return Mode::Normal };
        s.sent.insert(index);
        let left = total - s.sent.len();
        let label = self.pane_label(target);
        let next = if left > 0 {
            format!(" · {}: next step", self.runbooks_key())
        } else {
            " · that was the last step".into()
        };
        self.toast(client, format!("step {}/{total} sent to {label}{next}", index + 1));
        Mode::Normal
    }

    pub(in crate::server) fn copy_block(&mut self, client: ClientId, index: usize) {
        if let Some(code) = self.md_sessions.get(&client).and_then(|s| s.blocks.get(index)).map(|b| b.code.clone()) {
            self.copy_to_client(client, &code);
        }
    }
}
