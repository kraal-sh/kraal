//! Questions: text prompts and confirmations, and what their answers do.

use anyhow::Context;

use crate::input::{InputEvent, KeyCode, MouseButton, MouseKind, encode_paste};
use crate::pane::PaneSpec;
use crate::paths;
use crate::protocol::Placement;
use crate::server::definitions::file_name;
use crate::server::ui::{ConfirmAction, Mode, Prompt, PromptAction, Question};
use crate::server::{ClientId, Server};

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
                    if matches!(p.action, PromptAction::SaveWorkspace(_)) {
                        return self.save_workspace_prompt(id, p);
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
            PromptAction::SaveWorkspace(_) => unreachable!("answered in on_prompt"),
        }
        self.mark_persist();
        self.mark_dirty();
        Ok(())
    }

    fn save_workspace_prompt(&mut self, id: ClientId, mut p: Prompt) -> Mode {
        let PromptAction::SaveWorkspace(ws) = p.action else { unreachable!() };
        let Some(wi) = self.ws_index(ws) else {
            p.error = Some("This workspace is no longer open; cancel and choose a workspace.".into());
            return Mode::Prompt(p);
        };
        let name = p.value.trim();
        match self.prepare_definition_save(wi, name) {
            Ok(save) => {
                if let Some(question) = &save.question {
                    return Mode::confirm(
                        format!(
                            "{question}\nA backup will be kept as {}.\nCancel to choose another name.",
                            save.revision.target.with_extension("kdl.bak").display()
                        ),
                        ConfirmAction::ReplaceWorkspaceFile { prompt: p, save },
                    );
                }
                match self.write_definition(wi, name, &save) {
                    Ok(path) => {
                        self.toast(id, format!("saved {}", paths::shorten_home(&path)));
                        return Mode::Normal;
                    }
                    Err(e) => p.error = Some(format!("{e:#}")),
                }
            }
            Err(e) => p.error = Some(format!("{e:#}")),
        }
        Mode::Prompt(p)
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
            ConfirmAction::ReplaceWorkspaceFile { mut prompt, save } => {
                if !yes {
                    return Mode::Prompt(prompt);
                }
                let PromptAction::SaveWorkspace(ws) = prompt.action else { unreachable!() };
                let result = self
                    .ws_index(ws)
                    .context("This workspace is no longer open; cancel and choose a workspace.")
                    .and_then(|wi| self.write_definition(wi, prompt.value.trim(), &save));
                match result {
                    Ok(path) => {
                        self.toast(id, format!("saved {}", paths::shorten_home(&path)));
                        return Mode::Normal;
                    }
                    Err(e) => {
                        prompt.error = Some(format!("{e:#}"));
                        return Mode::Prompt(prompt);
                    }
                }
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
                    | ConfirmAction::ReplaceWorkspaceFile { .. }
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
                    ConfirmAction::RetryAccountSwitch { pane, account, undo } => {
                        let result = self
                            .accounts
                            .by_id(&account.id)
                            .filter(|a| a.same_home(&account))
                            .context("the target account changed; choose an account again")
                            .and_then(|a| {
                                if undo {
                                    self.undo_account_change(pane, id)
                                } else {
                                    self.switch_account(pane, &a, Some(id), false)
                                }
                            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, Mods};

    fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key(Key { code, mods: Mods::NONE, raw: Vec::new() })
    }

    #[test]
    fn replacing_a_file_defaults_to_cancel_and_keeps_the_prompt_on_failure() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let dir = std::env::temp_dir().join(format!("kraal-save-prompt-{}-{}", std::process::id(), "long-".repeat(16)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("work.kdl");
        std::fs::write(&path, "external contents").unwrap();
        let ws = server.new_workspace("work".into(), Some(path.clone()));
        let p = Prompt::new("Save workspace layout as", " work ", PromptAction::SaveWorkspace(ws));
        let Mode::Confirm(q) = server.on_prompt(0, p, key(KeyCode::Enter)) else { panic!("confirmation") };
        assert!(!q.yes);
        assert_eq!(q.action.verb(), "Replace file");
        for (cols, rows) in [(80, 24), (124, 36)] {
            let layout = crate::server::overlays::confirm_layout(&q, ratatui::layout::Rect::new(0, 0, cols, rows));
            assert!(layout.cancel.is_some() && layout.ok.is_some());
        }
        let Mode::Prompt(p) = server.on_confirm(0, q, key(KeyCode::Enter)) else { panic!("cancel returns to input") };
        assert_eq!(p.value, " work ");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external contents");
        let Mode::Confirm(q) = server.on_prompt(0, p, key(KeyCode::Enter)) else { panic!("confirmation") };
        std::fs::write(&path, "a second edit").unwrap();
        let Mode::Prompt(p) = server.on_confirm(0, q, key(KeyCode::Char('y'))) else { panic!("stale confirmation") };
        assert!(p.error.as_ref().unwrap().contains("changed while saving"));
        // The error's reason and next step stay visible even when its path fills the row.
        for (cols, rows) in [(80, 24), (124, 36)] {
            let screen = ratatui::layout::Rect::new(0, 0, cols, rows);
            let mut buf = ratatui::buffer::Buffer::empty(screen);
            crate::server::overlays::draw_prompt(&p, screen, &mut buf);
            let shown: String = buf.content.iter().map(|c| c.symbol()).collect();
            assert!(shown.contains("changed while saving; retry to review"), "{shown}");
        }
        assert_eq!(p.value, " work ");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a second edit");
        let Mode::Confirm(q) = server.on_prompt(0, p, key(KeyCode::Enter)) else { panic!("ask again") };
        assert!(matches!(server.on_confirm(0, q, key(KeyCode::Char('y'))), Mode::Normal));
        assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), "a second edit");
        // An I/O failure stays in the form, including its name and cursor.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let p = Prompt::new("Save workspace layout as", "work", PromptAction::SaveWorkspace(ws));
        let Mode::Prompt(p) = server.on_prompt(0, p, key(KeyCode::Enter)) else { panic!("keep the input") };
        assert_eq!((p.value.as_str(), p.cursor), ("work", 4));
        assert!(p.error.as_ref().unwrap().contains("not a regular file"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
