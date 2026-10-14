//! Edit a pane's saved launch, keeping its process until the change is confirmed and saved.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::panes::ProgramStop;
use super::persistence::ClosePane;
use super::ui::{ConfirmAction, Mode, Prompt, PromptAction};
use super::{ClientId, Server, forms, overlays};
use crate::ansi::Cursor;
use crate::input::InputEvent;
use crate::layout::PaneId;
use crate::pane::{PaneSpec, ProcessHold};
use crate::theme::NoticeKind;

pub struct PaneForm {
    pane: PaneId,
    values: [String; 3],
    cursors: [usize; 3],
    field: usize,
    scroll: usize,
    error: Option<String>,
    original: PaneSpec,
    name: Option<String>,
    folder: PathBuf,
    process: Option<ProcessHold>,
    approved: Option<ClosePane>,
}

pub(super) struct PaneEdit {
    form: PaneForm,
    client: ClientId,
    name: Option<String>,
    spec: PaneSpec,
    run: bool,
    stopping: Option<ProgramStop>,
}

fn command_text(spec: &PaneSpec) -> String {
    match spec.command.as_slice() {
        [shell, flag, script] if shell == "/bin/sh" && flag == "-c" => script.clone(),
        argv => crate::shell::join(argv),
    }
}

fn buttons() -> forms::Buttons<'static> {
    forms::Buttons {
        primary: forms::Button { field: 3, label: "Apply" },
        alternate: None,
        cancel: forms::Button { field: 4, label: "Cancel" },
    }
}

fn form_layout(form: &PaneForm, screen: Rect) -> forms::FormLayout {
    let mut message = "Command uses sh syntax. Leave it empty for your shell.\nRunning programs need confirmation. Stopped commands wait for Enter.".to_string();
    if form.original.account.is_some() || form.original.account_id.is_some() {
        message.push_str("\nChanging Command starts a plain command without this agent's account or conversation.");
    }
    forms::FormLayout::new(screen, 3, &message, form.error.as_deref(), &buttons())
}

pub(super) fn draw_form(form: &PaneForm, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let layout = form_layout(form, screen);
    overlays::frame(layout.area, "Edit pane", crate::theme::current().accent, buf);
    let mut cursor = None;
    for (i, label) in ["Name      ", "Folder    ", "Command   "].into_iter().enumerate() {
        cursor = cursor.or(forms::draw_text_field(
            layout.fields[i],
            label,
            &form.values[i],
            form.cursors[i],
            "",
            form.field == i,
            buf,
        ));
    }
    forms::draw_message(&layout, form.scroll, buf);
    forms::draw_buttons(&layout, &buttons(), form.field, None, if form.field == 4 { "Cancel" } else { "Apply" }, buf);
    cursor
}

impl Server {
    pub(super) fn rename_pane(&mut self, pane: PaneId, name: Option<String>) {
        if let Some(p) = self.panes.get_mut(&pane) {
            p.name = name.clone();
        }
        if let Some(edit) = self.pane_edits.get_mut(&pane) {
            edit.name = name;
        }
    }

    pub(super) fn edit_pane_form(&mut self, client: ClientId, pane: PaneId) -> Mode {
        let Some(p) = self.panes.get(&pane) else { return Mode::Normal };
        if !p.is_terminal() || p.spec.login.is_some() {
            return Mode::Prompt(Prompt::new(
                "Rename pane",
                p.name.as_deref().unwrap_or(""),
                PromptAction::RenamePane(pane),
            ));
        }
        if self.pane_edits.contains_key(&pane) {
            self.toast(client, NoticeKind::Notice, "Applying pane changes. Waiting for its programs to stop.");
            return Mode::Normal;
        }
        let folder =
            p.current_cwd().or_else(|| p.spec.cwd.clone()).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
        let values = [p.name.clone().unwrap_or_default(), crate::paths::shorten_home(&folder), command_text(&p.spec)];
        Mode::PaneForm(Box::new(PaneForm {
            pane,
            cursors: values.each_ref().map(|s| s.chars().count()),
            values,
            field: 0,
            scroll: 0,
            error: None,
            original: p.spec.clone(),
            name: p.name.clone(),
            folder,
            process: p.process_hold(),
            approved: None,
        }))
    }

    pub(super) fn on_pane_form(&mut self, client: ClientId, mut form: PaneForm, event: InputEvent) -> Mode {
        let layout = form_layout(&form, self.client_screen(client));
        let mut fields: Vec<_> = form
            .values
            .iter_mut()
            .zip(&mut form.cursors)
            .enumerate()
            .map(|(i, (value, cursor))| forms::Field {
                id: i,
                area: layout.fields[i],
                value: forms::FieldValue::Text { value, cursor },
            })
            .collect();
        let mut pending = None;
        let outcome = forms::FormState {
            field: &mut form.field,
            scroll: &mut form.scroll,
            error: &mut form.error,
            pending: &mut pending,
        }
        .handle(event, &mut fields, &buttons(), &layout);
        match outcome {
            forms::FormEvent::Cancel => Mode::Normal,
            forms::FormEvent::Submit => self.submit_pane_form(client, form, false),
            _ => Mode::PaneForm(Box::new(form)),
        }
    }

    fn pane_form_spec(&self, form: &PaneForm) -> Result<(PaneSpec, bool)> {
        let p = self.panes.get(&form.pane).context("This pane was closed. Cancel and choose another pane.")?;
        ensure!(
            !self.pane_edits.contains_key(&form.pane),
            "A pane edit is already being applied. Cancel and wait for it to finish."
        );
        ensure!(
            !self.switches.contains_key(&form.pane),
            "An account change is pending. Cancel it in Change account before editing this pane."
        );
        ensure!(
            !p.preparing && p.spec.pending_agent.is_none(),
            "This pane is still being prepared. Cancel and wait for it to finish."
        );
        ensure!(
            self.workspace_of(form.pane).is_some_and(|(wi, _)| self.workspaces[wi].kept.is_none()),
            "This workspace is closing. Cancel and reopen it before editing."
        );
        let same_process = match (&form.process, p.process_hold()) {
            (Some(before), Some(now)) => before.is(&now),
            (None, None) => true,
            _ => false,
        };
        ensure!(
            same_process
                && p.name == form.name
                && serde_json::to_value(&p.spec)? == serde_json::to_value(&form.original)?,
            "This pane changed while the form was open. Cancel and open Edit pane again."
        );
        ensure!(!form.values[0].chars().any(char::is_control), "Use a name without control characters.");
        let folder = crate::paths::expand_tilde(&form.values[1]);
        ensure!(!folder.as_os_str().is_empty(), "Type a folder.");
        let folder = if form.values[1] == crate::paths::shorten_home(&form.folder) {
            form.folder.clone()
        } else if folder.is_relative() {
            form.folder.join(folder)
        } else {
            folder
        };
        let command_changed = form.values[2] != command_text(&form.original);
        let folder_changed = folder != form.folder;
        if !command_changed && !folder_changed {
            return Ok((p.spec.clone(), false));
        }
        ensure!(
            !self.runs.iter().any(|r| r.items.iter().any(|i| i.pane == form.pane && i.state.pending())),
            "This pane belongs to a pending runbook target. Wait for it to finish before changing Command or Folder."
        );
        ensure!(
            !form.values[1].chars().any(char::is_control)
                && (!command_changed || !form.values[2].chars().any(char::is_control)),
            "Use one line per field, without control characters."
        );
        ensure!(folder.is_dir(), "Folder {} is not available. Choose an existing folder.", folder.display());
        std::fs::read_dir(&folder).context("Cannot access the folder. Choose an accessible folder.")?;
        let mut spec = if command_changed {
            let command = form.values[2].trim();
            let command = if command.is_empty() {
                Vec::new()
            } else {
                let check = std::process::Command::new("/bin/sh")
                    .args(["-n", "-c", command])
                    .output()
                    .context("Cannot check Command. Check that /bin/sh is available.")?;
                ensure!(
                    check.status.success(),
                    "Correct Command and apply again.\n{}",
                    String::from_utf8_lossy(&check.stderr).trim()
                );
                vec!["/bin/sh".into(), "-c".into(), command.into()]
            };
            PaneSpec { command, ..Default::default() }
        } else {
            if !p.suspended && (p.spec.account.is_some() || p.spec.account_id.is_some()) {
                let session = p
                    .agent
                    .session_id
                    .as_deref()
                    .or(p.spec.session.as_deref())
                    .context("The conversation is not ready to resume. Finish a turn and apply again.")?;
                let account = self
                    .pane_account(form.pane)
                    .context("The agent's account is unavailable. Restore it in Accounts before applying.")?;
                crate::accounts::check_resume(&account, session)
                    .context("Cannot verify the conversation. Fix its saved transcript before applying.")?;
            }
            match (!p.suspended).then(|| self.restart_spec(form.pane)).flatten() {
                Some((spec, true)) => spec,
                Some((_, false)) => anyhow::bail!(
                    "This agent cannot safely resume with the same command. Use Change account or enter an explicit Command."
                ),
                None => p.spec.clone(),
            }
        };
        spec.cwd = Some(folder);
        self.refresh_worktree_env(&mut spec);
        spec.check_program(&self.env.shell)
            .context("Cannot start this command. Correct Command or Folder and apply again.")?;
        Ok((spec, true))
    }

    pub(super) fn submit_pane_form(&mut self, client: ClientId, mut form: PaneForm, approved: bool) -> Mode {
        form.error = None;
        form.scroll = 0;
        let (spec, launch_changed) = match self.pane_form_spec(&form) {
            Ok(value) => value,
            Err(e) => {
                form.error = Some(format!("{e:#}"));
                return Mode::PaneForm(Box::new(form));
            }
        };
        let pane = form.pane;
        let p = &self.panes[&pane];
        let running = p.exit_code.is_none() && !p.suspended;
        let name = (!form.values[0].trim().is_empty()).then(|| form.values[0].trim().to_string());
        let inventory = ClosePane::read(p);
        if launch_changed
            && (running || inventory.running())
            && (!approved || form.approved.as_ref() != Some(&inventory))
        {
            let info = self.pane_info(pane);
            let program = p.foreground_argv();
            let mut stops = crate::persist::stops_with_workspace(
                &info,
                program.as_deref(),
                &p.background_jobs(),
                p.shell_waiting(),
            );
            for command in inventory.commands() {
                if !stops.contains(&command) {
                    stops.push(command);
                }
            }
            if stops.is_empty() {
                stops.push("the shell".into());
            }
            let mut message = format!(
                "This will stop:\n  {}\nUnsaved work in these programs may be lost.\nThen {}:\n  {}\nFolder: {}",
                stops.join("\n  "),
                if running { "run" } else { "save for Enter" },
                if spec.command.is_empty() { self.env.shell.clone() } else { command_text(&spec) },
                spec.cwd.as_ref().unwrap().display()
            );
            if (form.original.account.is_some() || form.original.account_id.is_some()) && spec.account.is_none() {
                message.push_str("\nThe new command will not use this agent's account or conversation.");
            }
            form.approved = Some(inventory);
            let Mode::Confirm(mut question) = Mode::confirm(message, ConfirmAction::ApplyPane(Box::new(form))) else {
                unreachable!()
            };
            question.title = Some("Apply command?".into());
            return Mode::Confirm(question);
        }
        if !launch_changed {
            let old = std::mem::replace(&mut self.panes.get_mut(&pane).unwrap().name, name);
            if let Err(error) = self.save_now() {
                self.panes.get_mut(&pane).unwrap().name = old;
                form.error = Some(self.pane_edit_save_error(&error));
                return Mode::PaneForm(Box::new(form));
            }
            self.mark_dirty();
            self.toast(client, NoticeKind::Notice, "Pane updated");
            return Mode::Normal;
        }
        // A crash during the stop restores the requested command waiting for Enter.
        self.pane_edits.insert(pane, PaneEdit { form, client, name, spec, run: running, stopping: None });
        if let Err(error) = self.save_now() {
            let mut form = self.pane_edits.remove(&pane).unwrap().form;
            form.error = Some(self.pane_edit_save_error(&error));
            return Mode::PaneForm(Box::new(form));
        }
        let inventory = ClosePane::read(&self.panes[&pane]);
        if (running || inventory.running()) && self.pane_edits[&pane].form.approved.as_ref() != Some(&inventory) {
            let edit = self.pane_edits.remove(&pane).unwrap();
            if let Err(error) = self.save_now() {
                let mut form = edit.form;
                form.error = Some(self.pane_edit_save_error(&error));
                return Mode::PaneForm(Box::new(form));
            }
            return self.submit_pane_form(client, edit.form, false);
        }
        let stop = ProgramStop::new(&self.panes[&pane]);
        if let Some(stop) = &stop {
            stop.signal(libc::SIGHUP);
        }
        self.pane_edits.get_mut(&pane).unwrap().stopping = stop;
        self.toast(client, NoticeKind::Notice, "Applying pane changes. Waiting for its programs to stop.");
        self.check_pane_edits();
        Mode::Normal
    }

    fn pane_edit_save_error(&mut self, error: &str) -> String {
        // A timed-out writer may still finish, so queue the original layout after it.
        let rollback = self.save_now().err();
        self.mark_persist();
        match rollback {
            Some(rollback) => format!(
                "Nothing was stopped. Couldn't save or confirm the rollback. Fix the save error and apply again.\n{error}\n{rollback}"
            ),
            None => format!("Nothing was changed or stopped. Fix the save error and apply again.\n{error}"),
        }
    }

    pub(crate) fn edited_pane_snapshot(&self, pane: PaneId) -> Option<crate::persist::PaneSnap> {
        let edit = self.pane_edits.get(&pane)?;
        let mut spec = edit.spec.clone();
        spec.startup_input = None;
        Some(crate::persist::PaneSnap {
            name: edit.name.clone(),
            spec,
            agent: None,
            agent_session: edit.spec.session.clone(),
            stopped: true,
            scrollback: self.scrollback.get(&pane).and_then(|s| s.file.clone()),
        })
    }

    pub(super) fn check_pane_edits(&mut self) {
        let closing: Vec<_> = self
            .pane_edits
            .keys()
            .copied()
            .filter(|&id| self.workspace_of(id).is_some_and(|(wi, _)| self.workspaces[wi].kept.is_some()))
            .collect();
        for id in closing {
            self.pane_edits.remove(&id);
        }
        let ready: Vec<_> = self
            .pane_edits
            .iter_mut()
            .filter_map(|(&id, edit)| {
                let stopped = edit.stopping.as_mut().is_none_or(ProgramStop::finished);
                (stopped
                    && self.panes.get(&id).is_none_or(|p| p.exit_code.is_some() && p.output_finished())
                    && !self.unparsed.contains_key(&id))
                .then_some(id)
            })
            .collect();
        for id in ready {
            let edit = self.pane_edits.remove(&id).unwrap();
            let Some(pane) = self.panes.get_mut(&id) else { continue };
            pane.name = edit.name;
            pane.spec = edit.spec.clone();
            pane.reported_cwd = None;
            pane.title.clear();
            pane.agent = crate::agent::AgentTracker { session_id: edit.spec.session.clone(), ..Default::default() };
            pane.close_at = None;
            self.limit_alerts.remove(&id);
            self.observed_agents.remove(&id);
            let result = if edit.run {
                let folder = edit.spec.cwd.as_ref().unwrap();
                (|| -> Result<()> {
                    ensure!(folder.is_dir(), "Folder {} is no longer available.", folder.display());
                    if edit.spec.account.is_some() || edit.spec.account_id.is_some() {
                        let account =
                            self.pane_account(id).context("The agent's account was removed while stopping.")?;
                        let session = edit.spec.session.as_deref().context("The conversation is unavailable.")?;
                        crate::accounts::check_resume(&account, session)?;
                    }
                    self.panes.get_mut(&id).unwrap().respawn(&self.env, &self.tx)
                })()
            } else {
                self.suspend_pane(id, edit.spec.clone(), "Ready to run")
            };
            if let Err(e) = result {
                let reason = format!("Could not start. Open Edit pane to correct it, or Enter to retry.\n{e:#}");
                let _ = self.suspend_pane(id, edit.spec, &reason);
                self.toast(edit.client, NoticeKind::Error, reason);
            } else {
                self.toast(
                    edit.client,
                    NoticeKind::Notice,
                    if edit.run { "Pane updated" } else { "Pane updated · Enter Run" },
                );
            }
            let ssh = self.panes.get(&id).and_then(|p| self.ssh_target_of(&p.spec));
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.ssh = ssh;
            }
            self.mark_persist();
            self.relayout();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{InputEvent, Key, KeySpec, MouseButton, MouseEvent, MouseKind};
    use std::time::{Duration, Instant};

    struct EditTest {
        server: Server,
        rx: super::super::EventReceiver,
        client: ClientId,
        pane: PaneId,
        dir: PathBuf,
        _peer: std::os::unix::net::UnixStream,
    }

    impl EditTest {
        fn new(stopped: bool) -> Self {
            let dir = std::env::temp_dir().join(format!("kraal-pane-edit-{}", crate::accounts::new_uuid()));
            std::fs::create_dir_all(dir.join("next folder")).unwrap();
            let dir = dir.canonicalize().unwrap();
            let (tx, rx) = super::super::event_channel(4096);
            let mut server = super::super::tests::server(tx);
            let (client, peer) = super::super::tests::client(&mut server);
            super::super::persistence::tests::writer(&mut server, Vec::new());
            let ws = server.clients[&client].workspace;
            let command = if stopped { "printf 'old output\\n'" } else { "printf 'old output\\n'; exec sleep 60" };
            let pane = server
                .spawn_pane_in(
                    ws,
                    None,
                    PaneSpec {
                        command: vec!["/bin/sh".into(), "-c".into(), command.into()],
                        cwd: Some(dir.clone()),
                        ..Default::default()
                    },
                    crate::protocol::Placement::Auto,
                    Some("tests".into()),
                )
                .unwrap();
            let mut t = Self { server, rx, client, pane, dir, _peer: peer };
            t.until(|t| {
                t.server.panes[&t.pane].text(None).contains("old output")
                    && if stopped {
                        t.server.panes[&t.pane].exit_code.is_some()
                    } else {
                        t.server.panes[&t.pane]
                            .pid()
                            .and_then(crate::process::argv)
                            .is_some_and(|argv| argv.first().is_some_and(|s| s.ends_with("sleep")))
                    }
            });
            t
        }

        fn form(&mut self) -> PaneForm {
            let Mode::PaneForm(form) = self.server.edit_pane_form(self.client, self.pane) else { panic!("form") };
            *form
        }

        fn pump(&mut self) {
            while let Ok(event) = self.rx.try_recv() {
                self.server.handle(event);
            }
            for pane in self.server.panes.keys().copied().collect::<Vec<_>>() {
                let mut data = Vec::new();
                while self.server.take_output(pane, &mut data, false) {
                    self.server.pane_output(pane, &data);
                }
            }
            self.server.finish_exits();
            self.server.check_pane_edits();
        }

        fn until(&mut self, done: impl Fn(&Self) -> bool) {
            let started = Instant::now();
            loop {
                self.pump();
                if done(self) {
                    break;
                }
                assert!(started.elapsed() < crate::testing::PATIENCE, "pane edit did not finish");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn apply(&mut self, form: PaneForm) -> Mode {
            self.server.submit_pane_form(self.client, form, false)
        }

        fn confirm(&mut self, mode: Mode) -> Mode {
            let Mode::Confirm(mut question) = mode else { panic!("confirmation") };
            assert!(!question.yes);
            question.yes = true;
            self.answer(question)
        }

        fn answer(&mut self, question: super::super::ui::Question) -> Mode {
            self.server.clients.get_mut(&self.client).unwrap().mode = Mode::Confirm(question);
            self.server.client_input(self.client, b"\r");
            std::mem::replace(&mut self.server.clients.get_mut(&self.client).unwrap().mode, Mode::Normal)
        }
    }

    impl Drop for EditTest {
        fn drop(&mut self) {
            for edit in self.server.pane_edits.values() {
                if let Some(stop) = &edit.stopping {
                    stop.signal(libc::SIGKILL);
                }
            }
            for pane in self.server.panes.values() {
                if let Some(stop) = ProgramStop::new(pane) {
                    stop.signal(libc::SIGKILL);
                }
            }
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    fn key(spec: &str) -> InputEvent {
        let key = KeySpec::parse(spec).unwrap();
        InputEvent::Key(Key { code: key.code, mods: key.mods, raw: Vec::new() })
    }

    fn form(mode: Mode) -> PaneForm {
        let Mode::PaneForm(form) = mode else { panic!("form") };
        *form
    }

    #[test]
    fn name_only_and_unchanged_forms_keep_the_process_and_launch() {
        let mut t = EditTest::new(false);
        let process = t.server.panes[&t.pane].process_hold().unwrap();
        let before = serde_json::to_value(&t.server.panes[&t.pane].spec).unwrap();
        let unchanged = t.form();
        assert!(matches!(t.apply(unchanged), Mode::Normal));
        let mut renamed = t.form();
        renamed.values[0] = "my tests".into();
        assert!(matches!(t.apply(renamed), Mode::Normal));
        assert!(t.server.panes[&t.pane].process_hold().unwrap().is(&process));
        assert_eq!(t.server.panes[&t.pane].name.as_deref(), Some("my tests"));
        assert_eq!(serde_json::to_value(&t.server.panes[&t.pane].spec).unwrap(), before);
        assert!(t.server.pane_edits.is_empty());
        t.server.panes.get_mut(&t.pane).unwrap().spec.command = vec!["/missing/program".into(), "multi\nline".into()];
        let mut draft = t.form();
        draft.values[0] = "still renameable".into();
        assert!(matches!(t.apply(draft), Mode::Normal));
        assert!(process.group_running());
    }

    #[test]
    fn a_live_change_needs_confirmation_and_cancel_keeps_every_field() {
        let mut t = EditTest::new(false);
        let process = t.server.panes[&t.pane].process_hold().unwrap();
        let mut draft = t.form();
        draft.values = ["renamed".into(), t.dir.join("next folder").display().to_string(), "printf replacement".into()];
        let values = draft.values.clone();
        let Mode::Confirm(question) = t.apply(draft) else { panic!("confirmation") };
        assert_eq!(question.title.as_deref(), Some("Apply command?"));
        assert_eq!(question.action.verb(), "Stop and apply");
        assert!(!question.yes);
        assert!(question.message.contains("sleep 60"));
        assert!(question.message.contains("printf replacement"));
        let back = form(t.answer(question));
        assert_eq!(back.values, values);
        assert!(process.group_running());
        assert_eq!(t.server.panes[&t.pane].name.as_deref(), Some("tests"));
        assert!(matches!(t.server.on_pane_form(t.client, back, key("esc")), Mode::Normal));
        assert!(process.group_running());
    }

    #[test]
    fn stopped_edits_stay_ready_and_restore_without_running() {
        let mut t = EditTest::new(true);
        let mut draft = t.form();
        let marker = t.dir.join("ran");
        draft.values[1] = t.dir.join("next folder").display().to_string();
        draft.values[2] = format!("pwd > {}; printf 'new output\\n'", crate::shell::quote(marker.to_str().unwrap()));
        let command = draft.values[2].clone();
        assert!(matches!(t.apply(draft), Mode::Normal));
        t.until(|t| t.server.pane_edits.is_empty() && t.server.panes[&t.pane].exit_code.is_some());
        let pane = &t.server.panes[&t.pane];
        assert!(pane.suspended);
        assert_eq!(command_text(&pane.spec), command);
        assert_eq!(pane.current_cwd(), Some(t.dir.join("next folder")));
        assert!(pane.text(None).contains("old output"));
        assert!(!marker.exists());
        let saved = serde_json::to_vec(&crate::persist::snapshot(&t.server)).unwrap();
        let snap = serde_json::from_slice(&saved).unwrap();
        t.server.close_pane(t.pane);
        t.server.apply_snapshot(snap).unwrap();
        t.pane = *t.server.panes.keys().next().unwrap();
        assert!(t.server.panes[&t.pane].suspended);
        assert_eq!(command_text(&t.server.panes[&t.pane].spec), command);
        assert!(!marker.exists());
        t.until(|t| t.server.panes[&t.pane].exit_code.is_some());
        let ws = t.server.workspaces.iter().find(|w| w.panes().contains(&t.pane)).unwrap().id;
        t.server.clients.get_mut(&t.client).unwrap().workspace = ws;
        t.server.client_input(t.client, b"\r");
        t.until(|t| t.server.panes[&t.pane].text(None).lines().any(|line| line.trim() == "new output"));
        assert_eq!(std::fs::read_to_string(marker).unwrap().trim(), t.dir.join("next folder").to_str().unwrap());
    }

    #[test]
    fn invalid_folder_and_command_keep_the_draft_and_stop_nothing() {
        let mut t = EditTest::new(false);
        for (field, value, error) in [
            (1, t.dir.join("missing").display().to_string(), "Choose an existing folder"),
            (1, String::new(), "Type a folder"),
            (2, "printf 'unfinished".into(), "Correct Command"),
            (2, "echo\0bad".into(), "control characters"),
        ] {
            let mut draft = t.form();
            draft.values[field] = value.clone();
            let back = form(t.apply(draft));
            assert_eq!(back.values[field], value);
            assert!(back.error.unwrap().contains(error));
            assert!(t.server.panes[&t.pane].process_hold().unwrap().group_running());
            assert!(t.server.pane_edits.is_empty());
        }
        let marker = t.dir.join("syntax-check-must-not-run");
        let mut draft = t.form();
        draft.values[2] = format!("echo $(touch {})", crate::shell::quote(marker.to_str().unwrap()));
        assert!(matches!(t.apply(draft), Mode::Confirm(_)));
        assert!(!marker.exists());
    }

    #[test]
    fn a_failed_save_rolls_back_the_name_and_refuses_to_stop() {
        let mut t = EditTest::new(false);
        t.server.save_now().unwrap();
        for command in [false, true] {
            let before = serde_json::to_value(crate::persist::snapshot(&t.server)).unwrap();
            let saved = super::super::persistence::tests::writer(&mut t.server, vec![Err("Disk full".into()), Ok(())]);
            let mut draft = t.form();
            draft.values[0] = "renamed".into();
            if command {
                draft.values[2] = "printf new".into();
            }
            let next = t.apply(draft);
            let back = form(if command { t.confirm(next) } else { next });
            assert!(back.error.as_deref().unwrap().contains("Nothing was changed or stopped"));
            assert_eq!(back.values[0], "renamed");
            assert!(t.server.panes[&t.pane].process_hold().unwrap().group_running());
            assert_eq!(serde_json::to_value(crate::persist::snapshot(&t.server)).unwrap(), before);
            assert_eq!(saved.lock().unwrap().last(), Some(&before));
        }
    }

    #[test]
    fn another_clients_changes_and_closed_panes_refuse_stale_forms() {
        let mut t = EditTest::new(true);
        let mut stale = t.form();
        stale.values[2] = "printf stale".into();
        let mut first = t.form();
        first.values[0] = "other client".into();
        t.apply(first);
        let back = form(t.apply(stale));
        assert!(back.error.unwrap().contains("pane changed"));
        let mut restarted = t.form();
        restarted.values[2] = "printf stale".into();
        t.server.panes.get_mut(&t.pane).unwrap().respawn(&t.server.env, &t.server.tx).unwrap();
        assert!(form(t.apply(restarted)).error.unwrap().contains("pane changed"));
        let closed = t.form();
        t.server.close_pane(t.pane);
        assert!(form(t.apply(closed)).error.unwrap().contains("pane was closed"));
    }

    #[test]
    fn changed_foreground_activity_requires_a_fresh_confirmation() {
        let mut t = EditTest::new(true);
        let p = t.server.panes.get_mut(&t.pane).unwrap();
        p.spec.command = vec!["/bin/sh".into()];
        p.respawn(&t.server.env, &t.server.tx).unwrap();
        t.until(|t| t.server.panes[&t.pane].foreground_argv().is_some());
        let mut draft = t.form();
        draft.values[2] = "printf replacement".into();
        let Mode::Confirm(q) = t.apply(draft) else { panic!("confirmation") };
        t.server.panes.get_mut(&t.pane).unwrap().write_input(b"sleep 60\n".to_vec());
        t.until(|t| {
            t.server.panes[&t.pane]
                .foreground_argv()
                .is_some_and(|argv| argv.first().is_some_and(|s| s.ends_with("sleep")))
        });
        let next = t.confirm(Mode::Confirm(q));
        assert!(matches!(next, Mode::Confirm(q) if !q.yes && q.message.contains("sleep 60")));
        assert!(t.server.panes[&t.pane].process_hold().unwrap().group_running());
    }

    #[test]
    fn a_confirmed_restart_saves_the_stopped_launch_and_keeps_identity_and_output() {
        let mut t = EditTest::new(false);
        let old = t.server.panes[&t.pane].process_hold().unwrap();
        let saved = super::super::persistence::tests::writer(&mut t.server, Vec::new());
        let mut draft = t.form();
        draft.values[2] = "printf 'replacement output\\n'; exec sleep 60".into();
        let next = t.apply(draft);
        assert!(matches!(t.confirm(next), Mode::Normal));
        let snap = saved.lock().unwrap()[0].clone();
        let serialized = snap.to_string();
        assert!(serialized.contains("replacement output"));
        assert!(serialized.contains("\"stopped\":true"));
        t.until(|t| {
            t.server.pane_edits.is_empty() && t.server.panes[&t.pane].text(None).contains("replacement output")
        });
        assert!(!old.group_running());
        assert_eq!(t.server.panes.len(), 1);
        assert_eq!(t.server.workspaces[0].tab().unwrap().focus, Some(t.pane));
        assert!(t.server.panes[&t.pane].text(None).contains("old output"));
        assert!(!t.server.panes[&t.pane].process_hold().unwrap().is(&old));
    }

    #[test]
    fn a_launch_failure_keeps_the_requested_command_and_can_be_edited_again() {
        let mut t = EditTest::new(false);
        let mut draft = t.form();
        draft.values[1] = t.dir.join("next folder").display().to_string();
        draft.values[2] = "printf replacement".into();
        let next = t.apply(draft);
        t.confirm(next);
        std::fs::remove_dir(t.dir.join("next folder")).unwrap();
        t.until(|t| t.server.pane_edits.is_empty() && t.server.panes[&t.pane].exit_code.is_some());
        assert!(t.server.panes[&t.pane].suspended);
        assert_eq!(command_text(&t.server.panes[&t.pane].spec), "printf replacement");
        assert!(t.server.panes[&t.pane].text(None).contains("Could not start"));
        assert_eq!(t.form().values[2], "printf replacement");
    }

    #[test]
    fn fields_errors_and_mouse_actions_fit_both_terminal_sizes() {
        let mut t = EditTest::new(true);
        for (cols, rows) in [(80, 24), (124, 36)] {
            let client = t.server.clients.get_mut(&t.client).unwrap();
            client.cols = cols;
            client.rows = rows;
            let mut draft = t.form();
            draft.error = Some(format!("{}final detail", "An error with readable details. ".repeat(150)));
            let screen = t.server.client_screen(t.client);
            let layout = form_layout(&draft, screen);
            let mut buf = Buffer::empty(screen);
            draw_form(&draft, screen, &mut buf);
            assert_eq!(buf[layout.body.as_position()].fg, crate::theme::current().text);
            assert_eq!(
                buf[(layout.body.x, layout.body.y + layout.error_start as u16)].fg,
                crate::theme::current().blocked
            );
            while draft.scroll < layout.max_scroll {
                draft = form(t.server.on_pane_form(t.client, draft, key("pagedown")));
            }
            let mut buf = Buffer::empty(screen);
            draw_form(&draft, screen, &mut buf);
            let text: String = crate::testing::buffer_text(&buf);
            for value in ["Name", "Folder", "Command", "Apply", "Cancel", "final detail", "PgUp/PgDn"] {
                assert!(text.contains(value), "missing {value} at {cols}x{rows}");
            }
            draft = form(t.server.on_pane_form(t.client, draft, key("tab")));
            assert_eq!(draft.field, 1);
            draft = form(t.server.on_pane_form(t.client, draft, key("shift+tab")));
            assert_eq!(draft.field, 0);
            draft = form(t.server.on_pane_form(t.client, draft, InputEvent::Paste(" café".as_bytes().to_vec())));
            assert_eq!(draft.values[0], "tests café");
            draft = form(t.server.on_pane_form(t.client, draft, InputEvent::Paste(vec![b'x'; 4097])));
            assert!(draft.error.is_some());
            let layout = form_layout(&draft, screen);
            let click = |area: Rect| {
                InputEvent::Mouse(MouseEvent {
                    kind: MouseKind::Press(MouseButton::Left),
                    col: area.x,
                    row: area.y,
                    mods: Default::default(),
                })
            };
            assert!(matches!(t.server.on_pane_form(t.client, draft, click(layout.primary)), Mode::Normal));
            let draft = t.form();
            let layout = form_layout(&draft, screen);
            assert!(matches!(t.server.on_pane_form(t.client, draft, click(layout.cancel)), Mode::Normal));
            t.server.panes.get_mut(&t.pane).unwrap().name = Some("tests".into());
        }
    }

    #[test]
    fn folder_changes_verify_and_resume_the_managed_conversation() {
        let mut t = EditTest::new(false);
        let home = t.dir.join("account");
        let transcript = home.join("projects/work/session.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let account: crate::accounts::Account = serde_json::from_value(serde_json::json!({
            "id": crate::accounts::new_uuid(), "agent": "claude", "name": "work", "home": home,
        }))
        .unwrap();
        t.server.accounts.add(account.clone()).unwrap();
        let executable = t.dir.join("claude");
        crate::testing::write_script(&executable, "#!/bin/sh\nprintf 'resumed: %s\\n' \"$*\"\nexec sleep 60\n");
        let mut spec = t.server.agent_spec(&account, Some(t.dir.clone()), Some("session"), &[]).unwrap();
        spec.command[0] = executable.display().to_string();
        t.server.panes.get_mut(&t.pane).unwrap().spec = spec;
        t.server.panes.get_mut(&t.pane).unwrap().agent.session_id = Some("session".into());
        let mut draft = t.form();
        draft.values[1] = t.dir.join("next folder").display().to_string();
        let back = form(t.apply(draft));
        assert!(back.error.as_deref().unwrap().contains("Cannot verify the conversation"));
        assert!(t.server.panes[&t.pane].process_hold().unwrap().group_running());
        std::fs::write(&transcript, "{\"message\":1}\n").unwrap();
        let (resume, changed) = t.server.pane_form_spec(&back).unwrap();
        assert!(changed);
        assert_eq!(resume.account_id.as_deref(), Some(account.id.as_str()));
        assert_eq!(resume.session.as_deref(), Some("session"));
        assert!(resume.command.windows(2).any(|a| a == ["--resume", "session"]));
        let next = t.apply(back);
        t.confirm(next);
        t.until(|t| {
            t.server.pane_edits.is_empty() && t.server.panes[&t.pane].text(None).contains("resumed: --resume session")
        });
        assert_eq!(t.server.panes[&t.pane].agent.session_id.as_deref(), Some("session"));
        assert_eq!(std::fs::read_to_string(transcript).unwrap(), "{\"message\":1}\n");
    }

    #[test]
    fn replacing_an_agent_command_clears_only_its_launch_identity_and_declares_the_change() {
        let mut t = EditTest::new(false);
        let p = t.server.panes.get_mut(&t.pane).unwrap();
        p.spec.account = Some("claude:work".into());
        p.spec.account_id = Some("account-id".into());
        p.spec.session = Some("session".into());
        p.spec.account_undo = Some(Box::new(PaneSpec::default()));
        p.spec.env = vec![("CLAUDE_CONFIG_DIR".into(), "/private/account".into())];
        p.spec.worktree = Some(crate::git::Worktree {
            repo: t.dir.clone(),
            path: t.dir.clone(),
            branch: "kraal/test".into(),
            base: "main".into(),
            index: 1,
        });
        p.spec.close_on_exit = true;
        let mut draft = t.form();
        draft.values[2] = "printf replacement".into();
        let layout = form_layout(&draft, Rect::new(0, 0, 80, 24));
        assert!(layout.lines.join(" ").contains("without this agent's account or conversation"));
        let (spec, _) = t.server.pane_form_spec(&draft).unwrap();
        assert!(spec.account.is_none() && spec.account_id.is_none() && spec.session.is_none());
        assert!(spec.account_undo.is_none());
        assert_eq!(spec.env, crate::worktree_setup::env("test", 1, &t.server.config.worktrees));
        assert_eq!(spec.worktree.as_ref().unwrap().path, t.dir);
        assert!(!spec.close_on_exit);
        assert!(
            matches!(t.apply(draft), Mode::Confirm(q) if q.message.contains("will not use this agent's account or conversation"))
        );
    }

    #[test]
    fn a_failed_placeholder_keeps_the_saved_command_and_folder() {
        let mut t = EditTest::new(true);
        let spec =
            PaneSpec { command: vec!["make".into()], cwd: Some(t.dir.join("next folder")), ..Default::default() };
        t.server.env.session = "invalid\0environment".into();
        assert!(t.server.suspend_pane(t.pane, spec.clone(), "Ready to run").is_err());
        let p = &t.server.panes[&t.pane];
        assert_eq!(p.spec.command, spec.command);
        assert_eq!(p.current_cwd(), spec.cwd);
        assert!(p.suspended && p.exit_code.is_some());
    }

    #[test]
    fn an_empty_command_waits_for_enter_even_after_restore() {
        let mut t = EditTest::new(true);
        let mut draft = t.form();
        draft.values[2].clear();
        t.apply(draft);
        t.until(|t| t.server.pane_edits.is_empty() && t.server.panes[&t.pane].exit_code.is_some());
        assert!(t.server.panes[&t.pane].suspended);
        let snap = crate::persist::snapshot(&t.server);
        t.server.close_pane(t.pane);
        t.server.apply_snapshot(snap).unwrap();
        assert!(t.server.panes.values().all(|p| p.suspended && p.spec.command.is_empty()));
    }

    #[test]
    fn a_second_edit_cannot_start_while_programs_are_stopping() {
        let mut t = EditTest::new(false);
        let mut second = t.form();
        second.values[2] = "printf second".into();
        let mut first = t.form();
        first.values[2] = "printf first".into();
        let next = t.apply(first);
        t.confirm(next);
        let back = form(t.apply(second));
        assert!(back.error.unwrap().contains("already being applied"));
        assert_eq!(t.server.pane_edits.len(), 1);
        let reply = t
            .server
            .request(
                t.client,
                crate::protocol::Request::RenamePane { pane: t.pane.to_string(), name: "latest name".into() },
            )
            .unwrap();
        assert!(reply.is_ok());
        assert_eq!(t.server.edited_pane_snapshot(t.pane).unwrap().name.as_deref(), Some("latest name"));
        t.until(|t| t.server.pane_edits.is_empty());
        assert_eq!(t.server.panes[&t.pane].name.as_deref(), Some("latest name"));
    }

    #[test]
    fn a_restart_waits_for_background_jobs_that_ignore_hangup() {
        let mut t = EditTest::new(true);
        let child_file = t.dir.join("child");
        let child = t.dir.join("child.sh");
        crate::testing::write_script(
            &child,
            &format!(
                "#!/bin/sh\ntrap '' HUP TERM\necho $$ > {}\nexec sleep 60\n",
                crate::shell::quote(child_file.to_str().unwrap())
            ),
        );
        let p = t.server.panes.get_mut(&t.pane).unwrap();
        p.spec.command =
            vec!["/bin/sh".into(), "-c".into(), format!("{} & wait", crate::shell::quote(child.to_str().unwrap()))];
        p.respawn(&t.server.env, &t.server.tx).unwrap();
        t.until(|_| crate::testing::read_complete_lines(&child_file).is_some());
        let child_pid: u32 = crate::testing::read_complete_lines(&child_file).unwrap().trim().parse().unwrap();
        let child_started = crate::process::start_time(child_pid).unwrap();
        let marker = t.dir.join("replacement");
        let mut draft = t.form();
        draft.values[2] = format!("echo new > {}", crate::shell::quote(marker.to_str().unwrap()));
        let next = t.apply(draft);
        t.confirm(next);
        t.pump();
        assert!(!marker.exists());
        assert!(crate::process::still_running(child_pid, child_started));
        t.server.pane_edits.get_mut(&t.pane).unwrap().stopping.as_mut().unwrap().since =
            Instant::now() - Duration::from_secs(3);
        t.until(|t| t.server.pane_edits.is_empty() && crate::testing::read_complete_lines(&marker).is_some());
        assert!(!crate::process::still_running(child_pid, child_started));
    }

    #[test]
    fn an_exited_leaders_surviving_child_requires_confirmation() {
        let mut t = EditTest::new(true);
        let p = t.server.panes.get_mut(&t.pane).unwrap();
        p.spec.command = vec!["/bin/sh".into(), "-c".into(), "trap '' HUP TERM; sleep 60 & exit 0".into()];
        p.respawn(&t.server.env, &t.server.tx).unwrap();
        let process = p.process_hold().unwrap();
        t.until(|t| t.server.panes[&t.pane].exit_code.is_some());
        assert!(process.group_running());
        let marker = t.dir.join("must-wait-for-enter");
        let mut draft = t.form();
        draft.values[2] = format!("touch {}", crate::shell::quote(marker.to_str().unwrap()));
        let Mode::Confirm(question) = t.apply(draft) else { panic!("confirmation for the surviving child") };
        assert!(question.message.contains("sleep 60"));
        let draft = form(t.answer(question));
        assert!(process.group_running());
        assert!(t.server.pane_edits.is_empty());
        let next = t.apply(draft);
        assert!(matches!(t.confirm(next), Mode::Normal));
        assert!(!t.server.pane_edits[&t.pane].run);
        t.server.pane_edits.get_mut(&t.pane).unwrap().stopping.as_mut().unwrap().since =
            Instant::now() - Duration::from_secs(3);
        t.until(|t| t.server.pane_edits.is_empty());
        assert!(!process.group_running());
        assert!(t.server.panes[&t.pane].suspended);
        assert!(!marker.exists());
    }

    #[test]
    fn an_edit_drains_output_printed_after_the_leaders_exit_deadline() {
        let mut t = EditTest::new(true);
        let release = t.dir.join("release-child");
        assert!(std::process::Command::new("mkfifo").arg(&release).status().unwrap().success());
        let end = if cfg!(target_os = "macos") { "wait" } else { "exit 0" };
        let p = t.server.panes.get_mut(&t.pane).unwrap();
        p.spec.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "trap '' HUP TERM; (printf 'child ready\\n'; read release < {}; printf 'last child output\\n') & {end}",
                crate::shell::quote(release.to_str().unwrap())
            ),
        ];
        p.respawn(&t.server.env, &t.server.tx).unwrap();
        let process = p.process_hold().unwrap();
        t.until(|t| t.server.panes[&t.pane].text(None).contains("child ready"));
        if cfg!(target_os = "macos") {
            // macOS revokes the controlling terminal when its leader exits. Deliver the exit
            // notification and expire its deadline while the real child can still print.
            let pid = t.server.panes[&t.pane].pid();
            t.server.handle(super::super::Event::PaneExited(t.pane, pid, Some(0)));
            t.server.exiting.get_mut(&t.pane).unwrap().2 = Instant::now();
            t.server.finish_exits();
        } else {
            t.until(|t| t.server.panes[&t.pane].exit_code.is_some());
        }
        assert_eq!(t.server.panes[&t.pane].exit_code, Some(Some(0)));
        assert!(!t.server.panes[&t.pane].output_finished());
        assert!(process.group_running());
        let mut draft = t.form();
        draft.values[2] = "true".into();
        let next = t.apply(draft);
        t.confirm(next);
        assert!(t.server.pane_edits.contains_key(&t.pane));
        std::fs::write(release, "ready\n").unwrap();
        let start = Instant::now();
        while process.group_running() || !t.server.panes[&t.pane].has_output() {
            assert!(start.elapsed() < crate::testing::PATIENCE);
            std::thread::sleep(Duration::from_millis(5));
        }
        t.server.check_pane_edits();
        assert!(t.server.pane_edits.contains_key(&t.pane), "the reader's output still waits");
        let mut data = Vec::new();
        assert!(t.server.take_output(t.pane, &mut data, false));
        t.server.unparsed.insert(t.pane, data);
        t.server.check_pane_edits();
        assert!(t.server.pane_edits.contains_key(&t.pane), "taken output must also be parsed");
        t.until(|t| t.server.pane_edits.is_empty());
        assert!(t.server.panes[&t.pane].text(None).contains("last child output"));
    }

    #[test]
    fn a_closing_workspace_cancels_an_edit_even_after_close_times_out() {
        for retry in [false, true] {
            let mut t = EditTest::new(false);
            let marker = t.dir.join("must-not-restart");
            let mut draft = t.form();
            draft.values[2] = format!("touch {}", crate::shell::quote(marker.to_str().unwrap()));
            let next = t.apply(draft);
            t.confirm(next);
            assert!(t.server.pane_edits.contains_key(&t.pane));
            let ws = t.server.clients[&t.client].workspace;
            if retry {
                // A saved Close intent that could not yet stop all programs owns this layout.
                let mut saved = crate::persist::snapshot(&t.server).workspaces.remove(0);
                saved.close_pending = Some(crate::persist::CloseIntent::new(Vec::new(), Vec::new()));
                t.server.workspaces[0].kept = Some(saved);
                t.server.save_now().unwrap();
                t.server.check_pane_edits();
            } else {
                t.server.keep_and_close_workspace(ws).unwrap();
            }
            assert!(t.server.pane_edits.is_empty());
            if retry {
                t.until(|t| t.server.panes[&t.pane].exit_code.is_some());
                assert_eq!(command_text(&t.server.panes[&t.pane].spec), "printf 'old output\\n'; exec sleep 60");
            }
            assert!(!marker.exists());
            let saved = serde_json::to_string(&crate::persist::snapshot(&t.server)).unwrap();
            assert!(saved.contains("must-not-restart"), "Close preserves the requested launch for reopening");
        }
    }

    #[test]
    fn both_launch_edits_refresh_worktree_identity_from_the_physical_folder() {
        let mut t = EditTest::new(true);
        let (a, b) = (t.dir.join("a"), t.dir.join("b"));
        std::fs::create_dir_all(a.join("src")).unwrap();
        std::fs::create_dir_all(b.join("src")).unwrap();
        std::os::unix::fs::symlink(&b, a.join("escape")).unwrap();
        std::os::unix::fs::symlink(&a, t.dir.join("alias")).unwrap();
        let worktree = |path: PathBuf, name: &str, index| crate::git::Worktree {
            repo: t.dir.clone(),
            path,
            branch: format!("kraal/{name}"),
            base: "main".into(),
            index,
        };
        let first = worktree(a.clone(), "a", 1);
        let second = worktree(b.clone(), "b", 2);
        t.server.config.worktrees.port_base = Some(8000);
        let p = t.server.panes.get_mut(&t.pane).unwrap();
        p.spec.cwd = Some(a.clone());
        p.spec.worktree = Some(first.clone());
        p.spec.env = crate::worktree_setup::env("a", 1, &t.server.config.worktrees);
        p.spec.env.push(("CUSTOM".into(), "kept".into()));
        t.server
            .spawn_pane(
                None,
                PaneSpec {
                    text: Some(crate::pane::ShownText { title: "worktree b".into(), text: String::new() }),
                    worktree: Some(second.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        for command in [false, true] {
            for (folder, expected) in [
                (b.join("src"), Some(&second)),
                (a.join("../b/src"), Some(&second)),
                (a.join("escape/src"), Some(&second)),
                (t.dir.join("alias/src"), Some(&first)),
                (a.join(".."), None),
            ] {
                let mut draft = t.form();
                draft.values[1] = folder.display().to_string();
                if command {
                    draft.values[2] = "true".into();
                }
                let (spec, changed) = t.server.pane_form_spec(&draft).unwrap();
                assert!(changed);
                assert_eq!(spec.worktree.as_ref().map(|w| &w.path), expected.map(|w| &w.path));
                let mut env = expected
                    .map(|w| crate::worktree_setup::env(&w.name(), w.index, &t.server.config.worktrees))
                    .unwrap_or_default();
                if !command {
                    env.insert(0, ("CUSTOM".into(), "kept".into()));
                }
                assert_eq!(spec.env, env, "folder {}", folder.display());
            }
        }
    }

    #[test]
    fn folder_only_edits_launch_relative_executables_with_a_slash() {
        let mut t = EditTest::new(true);
        for folder in [&t.dir, &t.dir.join("next folder")] {
            std::fs::create_dir(folder.join("bin")).unwrap();
            crate::testing::write_script(&folder.join("bin/tool"), "#!/bin/sh\npwd\n");
        }
        let p = t.server.panes.get_mut(&t.pane).unwrap();
        p.spec.command = vec!["bin/tool".into()];
        p.respawn(&t.server.env, &t.server.tx).unwrap();
        t.until(|t| t.server.panes[&t.pane].exit_code.is_some());
        assert_eq!(t.server.panes[&t.pane].exit_code, Some(Some(0)));
        let mut draft = t.form();
        draft.values[1] = "next folder".into();
        assert!(matches!(t.apply(draft), Mode::Normal));
        t.until(|t| t.server.pane_edits.is_empty() && t.server.panes[&t.pane].exit_code.is_some());
        t.server.client_input(t.client, b"\r");
        t.until(|t| t.server.panes[&t.pane].text(None).contains("next folder"));
        t.until(|t| t.server.panes[&t.pane].exit_code.is_some());
        assert_eq!(t.server.panes[&t.pane].exit_code, Some(Some(0)));
        assert_eq!(t.server.panes[&t.pane].spec.command, ["bin/tool"]);
    }

    #[test]
    fn folder_whitespace_survives_unchanged_forms_renames_and_launch_edits() {
        let mut t = EditTest::new(true);
        let folder = t.dir.join("project ");
        std::fs::create_dir(&folder).unwrap();
        for trimmed_exists in [false, true] {
            if trimmed_exists {
                std::fs::create_dir(t.dir.join("project")).unwrap();
            }
            t.server.panes.get_mut(&t.pane).unwrap().spec.cwd = Some(folder.clone());
            let before = serde_json::to_value(&t.server.panes[&t.pane].spec).unwrap();
            for name in [None, Some("renamed")] {
                let mut draft = t.form();
                assert!(draft.values[1].ends_with("project "));
                if let Some(name) = name {
                    draft.values[0] = name.into();
                }
                assert!(matches!(t.apply(draft), Mode::Normal));
                assert!(t.server.pane_edits.is_empty());
                assert_eq!(serde_json::to_value(&t.server.panes[&t.pane].spec).unwrap(), before);
            }
            let mut draft = t.form();
            draft.values[2] = "true".into();
            assert_eq!(t.server.pane_form_spec(&draft).unwrap().0.cwd, Some(folder.clone()));
        }
        t.server.panes.get_mut(&t.pane).unwrap().spec.cwd = Some(t.dir.clone());
        let mut draft = t.form();
        draft.values[1] = "project ".into();
        assert_eq!(t.server.pane_form_spec(&draft).unwrap().0.cwd, Some(folder));
    }

    #[test]
    fn folder_only_keeps_literal_arguments_and_checks_the_program_before_stopping() {
        let mut t = EditTest::new(false);
        t.server.panes.get_mut(&t.pane).unwrap().spec.command =
            vec!["/bin/sh".into(), "-c".into(), "printf '%s' \"$1\"".into(), "sh".into(), "spaces ' and $HOME".into()];
        let before = t.server.panes[&t.pane].spec.command.clone();
        let mut draft = t.form();
        draft.values[1] = "next folder".into();
        let (spec, changed) = t.server.pane_form_spec(&draft).unwrap();
        assert!(changed);
        assert_eq!(spec.command, before);
        assert_eq!(spec.cwd, Some(t.dir.join("next folder")));
        t.server.panes.get_mut(&t.pane).unwrap().spec.command[0] = t.dir.join("missing-program").display().to_string();
        let mut draft = t.form();
        draft.values[1] = "next folder".into();
        assert!(form(t.apply(draft)).error.unwrap().contains("Cannot start this command"));
        assert!(t.server.panes[&t.pane].process_hold().unwrap().group_running());
    }

    #[test]
    fn previews_offer_the_existing_rename_action() {
        let mut t = EditTest::new(true);
        let pane = t
            .server
            .spawn_pane(
                None,
                PaneSpec {
                    text: Some(crate::pane::ShownText { title: "preview".into(), text: "kept".into() }),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            matches!(t.server.edit_pane_form(t.client, pane), Mode::Prompt(p) if matches!(p.action, PromptAction::RenamePane(id) if id == pane))
        );
    }

    #[test]
    fn closing_a_pane_during_apply_never_recreates_it() {
        let mut t = EditTest::new(false);
        let marker = t.dir.join("must-not-run");
        let mut draft = t.form();
        draft.values[2] = format!("echo new > {}", crate::shell::quote(marker.to_str().unwrap()));
        let next = t.apply(draft);
        t.confirm(next);
        t.server.close_pane(t.pane);
        t.until(|t| t.server.pane_edits.is_empty());
        assert!(t.server.panes.is_empty());
        assert!(!marker.exists());
        assert_eq!(t.server.workspaces.len(), 1);
        t.server.client_input(t.client, b"\x02Pe");
        assert!(t.server.clients[&t.client].toast.as_ref().unwrap().0.contains("Add a pane before editing"));
    }

    #[test]
    fn account_changes_and_launch_edits_reserve_the_same_pane() {
        let mut t = EditTest::new(false);
        let account = |name: &str| {
            serde_json::from_value::<crate::accounts::Account>(serde_json::json!({
                "id": crate::accounts::new_uuid(), "agent": "claude", "name": name, "home": t.dir.join(name),
            }))
            .unwrap()
        };
        let (from, to) = (account("from"), account("to"));
        t.server.accounts.add(from.clone()).unwrap();
        t.server.accounts.add(to.clone()).unwrap();
        let p = t.server.panes.get_mut(&t.pane).unwrap();
        p.spec.account = Some("claude:from".into());
        p.spec.account_id = Some(from.id.clone());
        p.spec.command = vec!["claude".into()];
        let mut draft = t.form();
        draft.values[2] = "printf replacement".into();
        t.server.switch_account(t.pane, &to, Some(t.client), true).unwrap();
        let back = form(t.apply(draft));
        assert!(back.error.as_deref().unwrap().contains("account change is pending"));
        t.server.switches.remove(&t.pane);
        let next = t.apply(back);
        t.confirm(next);
        let error = t.server.switch_account(t.pane, &to, Some(t.client), true).unwrap_err();
        assert!(error.to_string().contains("pane edit is already being applied"));
    }
}
