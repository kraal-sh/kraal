//! Questions: text prompts and confirmations, and what their answers do.

use std::ffi::OsStr;
use std::path::PathBuf;

use anyhow::Context;

use crate::input::{InputEvent, KeyCode, MouseButton, MouseKind, encode_paste};
use crate::pane::PaneSpec;
use crate::protocol::Placement;
use crate::server::definitions::file_name;
use crate::server::ui::{ConfirmAction, Mode, Prompt, PromptAction, Question};
use crate::server::{ClientId, Server};
use crate::{paths, workspace};

impl Server {
    pub(super) fn on_prompt(&mut self, id: ClientId, mut p: Prompt, ev: InputEvent) -> Mode {
        match ev {
            InputEvent::Key(k) => match k.code {
                KeyCode::Esc => return self.cancel_account_prompt(&p),
                KeyCode::Char('c' | 'g') if k.mods.ctrl => return self.cancel_account_prompt(&p),
                // Files dropped on an SSH pane: paste their paths instead of uploading them.
                KeyCode::Char('p') if k.mods.ctrl && matches!(p.action, PromptAction::Upload { .. }) => {
                    if let PromptAction::Upload { raw, pane, .. } = p.action
                        && let Some(pn) = self.panes.get_mut(&pane)
                    {
                        let bytes = encode_paste(&raw, pn.input_modes());
                        pn.write_input(bytes);
                    }
                    return Mode::Normal;
                }
                KeyCode::Enter => {
                    if matches!(p.action, PromptAction::RunbookValue) {
                        return self.set_draft_value(id, p.value);
                    }
                    let title = p.title.clone();
                    let (value, cursor, masked) = (p.value.clone(), p.cursor, p.masked);
                    let renamed = match &p.action {
                        PromptAction::RenameAccount { agent, account } => {
                            self.accounts.get(agent, account).map(|a| a.id)
                        }
                        _ => None,
                    };
                    return match self.commit_prompt(id, p) {
                        Ok(()) => renamed.map_or(Mode::Normal, |a| Mode::Picker(self.accounts_picker(Some(&a)))),
                        // Not accepted: the prompt stays, with the reason under it.
                        Err((action, why)) => {
                            Mode::Prompt(Prompt { title, value, cursor, action, error: Some(why), masked })
                        }
                    };
                }
                _ => {
                    if crate::server::ui::edit_line(&mut p.value, &mut p.cursor, &k) {
                        p.error = None;
                    }
                }
            },
            InputEvent::Paste(data) => {
                p.error =
                    crate::server::ui::paste_line(&mut p.value, &mut p.cursor, &String::from_utf8_lossy(&data)).err();
            }
            _ => {}
        }
        Mode::Prompt(p)
    }

    fn cancel_account_prompt(&self, prompt: &Prompt) -> Mode {
        if let PromptAction::RenameAccount { agent, account } = &prompt.action {
            let a = self.accounts.get(agent, account);
            Mode::Picker(self.accounts_picker(a.as_ref().map(|a| a.id.as_str())))
        } else {
            Mode::Normal
        }
    }

    /// Act on a prompt's value; a value that cannot be used gives the action back with why.
    pub(super) fn commit_prompt(&mut self, id: ClientId, p: Prompt) -> Result<(), (PromptAction, String)> {
        let value = p.value.trim().to_string();
        match p.action {
            // Answered in on_prompt, which goes on to the next question.
            PromptAction::RunbookValue => {}
            PromptAction::RenameTab { workspace, tab } => {
                // An empty name gives the tab back its automatic one (its focused pane's).
                if let Some(wi) = self.ws_index(workspace)
                    && let Some(t) = self.workspaces[wi].tabs.iter_mut().find(|t| t.id == tab)
                {
                    t.name = value;
                }
            }
            PromptAction::RenamePane(pane) => {
                if let Some(pn) = self.panes.get_mut(&pane) {
                    pn.name = (!value.is_empty()).then_some(value);
                }
            }
            PromptAction::RenameWorkspace(ws) => {
                if value.is_empty() {
                    return Err((p.action, "Type a name.".into()));
                }
                if self.workspaces.iter().any(|w| w.name == value && w.id != ws) {
                    return Err((p.action, format!("A workspace named \"{value}\" is already open.")));
                }
                let Some(wi) = self.ws_index(ws) else { return Ok(()) };
                match self.rename_definition(wi, &value) {
                    Ok(Some(old)) => self.toast(id, format!("renamed {} to {value}.kdl", file_name(&old))),
                    Ok(None) => {}
                    Err(e) => return Err((p.action, format!("{e:#}"))),
                }
                self.workspaces[wi].name = value;
            }
            PromptAction::NewWorkspace => {
                if value.is_empty() {
                    return Err((p.action, "Type a name.".into()));
                }
                if self.workspaces.iter().any(|w| w.name == value) {
                    return Err((p.action, format!("A workspace named \"{value}\" is already open.")));
                }
                let ws = self.new_workspace(value, None);
                match self.spawn_pane_in(ws, None, PaneSpec::default(), Placement::Tab, None) {
                    Ok(_) => self.show_workspace(id, ws),
                    Err(e) => {
                        self.drop_empty_workspaces();
                        self.toast(id, format!("{e:#}"));
                    }
                }
            }
            PromptAction::OpenPreview => {
                if value.is_empty() {
                    return Err((p.action, "Type the path of a file or directory.".into()));
                }
                let base = self.client_focus(id).and_then(|p| self.panes.get(&p)).and_then(|p| p.current_cwd());
                let mut path = paths::expand_tilde(&value);
                if path.is_relative()
                    && let Some(b) = base
                {
                    path = b.join(path);
                }
                if !path.exists() {
                    return Err((p.action, format!("{} does not exist.", paths::shorten_home(&path))));
                }
                let Some(ws) = self.clients.get(&id).map(|c| c.workspace) else { return Ok(()) };
                let spec = PaneSpec { preview: Some(path), ..Default::default() };
                if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Auto, None) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PromptAction::Upload { paths, .. } => {
                let mut args: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                args.push(value);
                {
                    args.insert(0, "--".into());
                    args.insert(0, "cp".into());
                    self.start_job(id, "upload", args);
                }
            }
            PromptAction::UploadPick { server } => {
                let base = dirs::home_dir().unwrap_or_default();
                let mut args: Vec<String> = Vec::new();
                for word in value.split_whitespace() {
                    let p = paths::expand_tilde(word);
                    args.push(if p.is_relative() { base.join(p) } else { p }.display().to_string());
                }
                if args.is_empty() {
                    return Err((PromptAction::UploadPick { server }, "Type the local paths to upload.".into()));
                }
                args.push(format!("{server}:~/"));
                {
                    args.insert(0, "--".into());
                    args.insert(0, "cp".into());
                    self.start_job(id, "upload", args);
                }
            }
            PromptAction::Download => {
                if !value.contains(':') {
                    return Err((p.action, "Type server:path, the file to download.".into()));
                }
                let dir = paths::expand_tilde(&self.config.servers.download_dir);
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    return Err((p.action, format!("{}: {e}", dir.display())));
                }
                self.start_job(id, "download", vec!["cp".into(), "--".into(), value, format!("{}/", dir.display())]);
            }
            PromptAction::NewWorktree { agent, account } => {
                if let Err(e) = self.agent_in_worktree(id, &agent, &account, &value) {
                    return Err((PromptAction::NewWorktree { agent, account }, format!("{e:#}")));
                }
            }
            PromptAction::Commit { pane } => self.commit_worktree(id, pane, &value),
            PromptAction::Tunnel { server } => match self.add_tunnel(&server, &value, None) {
                Ok(t) => self.toast(id, format!("tunnel {}", t.spec.describe())),
                Err(e) => return Err((PromptAction::Tunnel { server }, format!("{e:#}"))),
            },
            PromptAction::RunOn { targets } => {
                if value.is_empty() {
                    return Err((PromptAction::RunOn { targets }, "Type the command to run.".into()));
                }
                let Some(ws) = self.clients.get(&id).map(|c| c.workspace) else { return Ok(()) };
                let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "kraal".into());
                let mut command = vec![exe, "exec".to_string()];
                command.extend(targets.iter().cloned());
                command.push("--".into());
                command.push(value.clone());
                let spec = PaneSpec { command, ..Default::default() };
                let label = format!("» {} on {}", value.chars().take(30).collect::<String>(), targets.join(" "));
                if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(label)) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PromptAction::RenameAccount { agent, account } => {
                if let Err(e) = self.rename_account(&agent, &account, &value) {
                    return Err((PromptAction::RenameAccount { agent, account }, format!("{e:#}")));
                }
                self.toast(id, format!("renamed account to {agent}·{value}"));
            }
            PromptAction::SaveWorkspace(ws) => {
                if !workspace::valid_file_name(&value) {
                    return Err((p.action, format!("{}: the layout is not saved yet.", workspace::FILE_NAME_RULE)));
                }
                let Some(wi) = self.ws_index(ws) else { return Ok(()) };
                match self.save_definition(wi, &value) {
                    Ok(path) => self.toast(id, format!("saved {}", paths::shorten_home(&path))),
                    Err(e) => self.toast(id, format!("{e:#}")),
                }
            }
        }
        self.mark_persist();
        self.mark_dirty();
        Ok(())
    }

    /// Save workspace `wi` as definition `name`, keeping the file it replaces as `.kdl.bak`: in
    /// its own definition file when `name` is that file's, else in the definitions directory. The
    /// name must be a plain file name: a CLI or protocol request must not write anywhere else.
    pub(in crate::server) fn save_definition(&mut self, wi: usize, name: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(workspace::valid_file_name(name), "{}, not {name:?}", workspace::FILE_NAME_RULE);
        let def = self.snapshot_definition(wi, name);
        let ws = &self.workspaces[wi];
        let own = ws.definition.clone().filter(|own| own.file_stem() == Some(OsStr::new(name)));
        // Under another name, a workspace that has a file of its own saves a copy.
        let link = own.is_some() || ws.definition.is_none();
        let path = match own {
            Some(own) => own,
            None => {
                let dir = self.definition_dir();
                std::fs::create_dir_all(&dir)?;
                dir.join(format!("{name}.kdl"))
            }
        };
        workspace::save_file(&path, &def)?;
        if link {
            self.workspaces[wi].set_definition(Some(&path));
        }
        Ok(path)
    }

    pub(super) fn on_confirm(&mut self, id: ClientId, mut q: Question, ev: InputEvent) -> Mode {
        let layout = crate::server::overlays::confirm_layout(&q, self.client_screen(id));
        let yes = match ev {
            InputEvent::Key(k) => {
                let plain = !k.mods.ctrl && !k.mods.alt;
                match k.code {
                    // Choose a button; Enter takes the selected one (Cancel at first).
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab if plain => {
                        q.yes = !q.yes;
                        return Mode::Confirm(q);
                    }
                    // A long question scrolls, with the keys that scroll the help: by a row, or by
                    // as many as the box shows, so that paging through it skips none.
                    KeyCode::Up | KeyCode::Char('k') if plain => {
                        q.scroll = q.scroll.saturating_sub(1);
                        return Mode::Confirm(q);
                    }
                    KeyCode::Down | KeyCode::Char('j') if plain => {
                        q.scroll = (q.scroll + 1).min(layout.max_scroll);
                        return Mode::Confirm(q);
                    }
                    KeyCode::PageUp if plain => {
                        q.scroll = q.scroll.saturating_sub(layout.page());
                        return Mode::Confirm(q);
                    }
                    KeyCode::PageDown | KeyCode::Char(' ') if plain => {
                        q.scroll = (q.scroll + layout.page()).min(layout.max_scroll);
                        return Mode::Confirm(q);
                    }
                    KeyCode::Home | KeyCode::Char('g') if plain => {
                        q.scroll = 0;
                        return Mode::Confirm(q);
                    }
                    KeyCode::Enter if plain => q.yes,
                    KeyCode::Char('y' | 'Y') if plain => true,
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => false,
                    KeyCode::Char('c' | 'g') if k.mods.ctrl => false,
                    // Anything else does not answer.
                    _ => return Mode::Confirm(q),
                }
            }
            InputEvent::Mouse(m) => {
                let at = ratatui::layout::Position::new(m.col, m.row);
                let hit = |r: Option<ratatui::layout::Rect>| r.is_some_and(|r| r.contains(at));
                match m.kind {
                    MouseKind::Press(MouseButton::Left) if hit(layout.cancel) => false,
                    MouseKind::Press(MouseButton::Left) if hit(layout.ok) => true,
                    MouseKind::ScrollDown => {
                        q.scroll = (q.scroll + 2).min(layout.max_scroll);
                        return Mode::Confirm(q);
                    }
                    MouseKind::ScrollUp => {
                        q.scroll = q.scroll.saturating_sub(2);
                        return Mode::Confirm(q);
                    }
                    _ => return Mode::Confirm(q),
                }
            }
            _ => return Mode::Confirm(q),
        };
        // A yes needs the question to have been readable.
        if yes && (layout.body == 0 || layout.cancel.is_none()) {
            self.toast(id, "The terminal is too small to show this question: make it larger to answer.");
            return Mode::Confirm(q);
        }
        let action = q.action;
        match action {
            ConfirmAction::RemoveAccount(a) => {
                if yes {
                    match crate::accounts::Accounts::update(|all| Ok(all.remove(&a.agent, &a.key()))) {
                        Ok(_) => {
                            self.reload_accounts();
                            self.toast(id, "Account removed");
                        }
                        Err(e) => self.toast(id, format!("{e:#}")),
                    }
                }
                return Mode::Picker(self.accounts_picker(Some(&a.id)));
            }
            ConfirmAction::UseExisting { existing, target } => {
                if yes {
                    if let Err(e) = self.confirm_existing_account(id, &existing, target.as_ref()) {
                        self.toast(id, format!("{e:#}"));
                        self.reload_accounts();
                        return Mode::Picker(self.accounts_picker(None));
                    }
                    return Mode::Picker(self.accounts_picker(None));
                }
                return Mode::Picker(self.review_account_picker(&existing));
            }
            ConfirmAction::StopAndSwitch { pane, token } => {
                self.confirm_switch(pane, token, id, yes);
                return Mode::Normal;
            }
            ConfirmAction::Production { workspace, approve, pending } => {
                return self.answer_production(id, yes, workspace, approve, pending);
            }
            ConfirmAction::RunRunbook => {
                self.confirm_runbook(id, yes);
                return Mode::Normal;
            }
            ConfirmAction::RunBlock { index, production } => return self.confirm_block(id, index, production, yes),
            ConfirmAction::TrustRunbook => return self.confirm_trust(id, yes),
            ConfirmAction::TrustWorktreeSetup(new) => {
                self.confirm_worktree_setup(id, *new, yes);
                return Mode::Normal;
            }
            ConfirmAction::RunTool { pane, name, selection, reason, place } => {
                if !yes {
                    return Mode::Normal;
                }
                // Asked about one production target, in one place: if the pane is now on another
                // (read again first), ask again; if it moved, nothing starts.
                return self.tool_request_after(id, pane, &name, selection, Some(reason), place);
            }
            _ => {}
        }
        match yes {
            true => {
                match action {
                    ConfirmAction::RemoveAccount(_)
                    | ConfirmAction::UseExisting { .. }
                    | ConfirmAction::StopAndSwitch { .. }
                    | ConfirmAction::Production { .. }
                    | ConfirmAction::RunRunbook
                    | ConfirmAction::RunBlock { .. }
                    | ConfirmAction::TrustRunbook
                    | ConfirmAction::TrustWorktreeSetup(_)
                    | ConfirmAction::RunTool { .. } => {
                        unreachable!("answered above")
                    }
                    ConfirmAction::Edit(target) => self.start_edit(Some(id), *target, None),
                    ConfirmAction::ClosePane(p) => self.close_pane(p),
                    ConfirmAction::CloseTab { workspace, tab } => {
                        if let Some(wi) = self.ws_index(workspace)
                            && let Some(t) = self.workspaces[wi].tabs.iter().find(|t| t.id == tab)
                        {
                            for p in t.panes() {
                                self.close_pane(p);
                            }
                        }
                    }
                    ConfirmAction::CloseWorkspace(ws) => self.close_workspace(ws),
                    ConfirmAction::Restart => self.restart(id),
                    ConfirmAction::Quit => self.quit(),
                    ConfirmAction::Job { label, args } => self.start_job(id, &label, args),
                    ConfirmAction::GitJob { label, script } => {
                        self.git_job(id, &label, script, crate::server::worktrees::JobEnd::CloseOnSuccess)
                    }
                    ConfirmAction::RemoveWorktrees(list) => self.remove_worktrees(id, list),
                    ConfirmAction::RetryAccountSwitch { pane, account } => {
                        let result = self
                            .accounts
                            .by_id(&account.id)
                            .filter(|a| a.same_home(&account))
                            .context("the target account changed; choose an account again")
                            .and_then(|a| self.switch_account(pane, &a, Some(id), false));
                        if let Err(e) = result {
                            self.toast(id, format!("{e:#}"));
                        }
                    }
                    ConfirmAction::SignInAccount(account) => {
                        let result = self
                            .accounts
                            .by_id(&account.id)
                            .filter(|a| a.same_home(&account))
                            .context("the target account changed; choose an account again")
                            .and_then(|a| self.open_login(id, &a));
                        if let Err(e) = result {
                            self.toast(id, format!("{e:#}"));
                        }
                    }
                }
                Mode::Normal
            }
            false => Mode::Normal,
        }
    }
}
