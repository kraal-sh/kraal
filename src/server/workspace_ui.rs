//! Workspace names use the same fields, buttons and return path as other management forms.

use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::ui::{Mode, Picker, PickerKind, PickerState, Prompt, PromptAction};
use super::{ClientId, Server, forms, overlays};
use crate::ansi::Cursor;
use crate::input::InputEvent;
use crate::pane::PaneSpec;
use crate::protocol::Placement;

#[derive(Default)]
pub struct WorkspaceForm {
    field: usize,
    pending: Option<u32>,
    cwd: Option<PathBuf>,
    back: Option<PickerState>,
    invalid_name: bool,
    file: String,
    file_cursor: usize,
    preview: Option<Box<crate::workspace::ImportPreview>>,
    choices: Vec<usize>,
    account: usize,
    existing: Option<u32>,
    result: Option<String>,
}

fn verb(p: &Prompt) -> &'static str {
    if p.workspace_form.as_ref().is_some_and(|f| f.result.is_some()) {
        return "Done";
    }
    if matches!(p.action, PromptAction::ExportWorkspace(_)) {
        if p.error.is_some() { "Retry" } else { "Export" }
    } else if matches!(p.action, PromptAction::ImportWorkspace) {
        let form = p.workspace_form.as_ref().unwrap();
        if form.existing.is_some() {
            "Open"
        } else if p.error.is_some() {
            "Retry"
        } else if form.preview.as_ref().is_some_and(|p| form.account + 1 < p.accounts.len()) {
            "Next"
        } else if form.preview.is_some() {
            "Add"
        } else {
            "Preview"
        }
    } else if matches!(p.action, PromptAction::RestoreWorkspace(_)) {
        "Restore"
    } else if p.error.is_some() && !p.workspace_form.as_ref().is_some_and(|f| f.invalid_name) {
        "Retry"
    } else if matches!(p.action, PromptAction::NewWorkspace) {
        "Add"
    } else if matches!(p.action, PromptAction::DuplicateWorkspace(_)) {
        "Duplicate"
    } else {
        "Rename"
    }
}

fn field_count(p: &Prompt) -> usize {
    if p.workspace_form.as_ref().is_some_and(|f| f.result.is_some()) {
        return 0;
    }
    if matches!(p.action, PromptAction::ImportWorkspace) {
        2 + p
            .workspace_form
            .as_ref()
            .and_then(|f| f.preview.as_ref())
            .map_or(0, |p| usize::from(!p.accounts.is_empty()))
    } else {
        1
    }
}

fn buttons(p: &Prompt) -> forms::Buttons<'static> {
    let count = field_count(p);
    forms::Buttons {
        primary: forms::Button { field: count, label: verb(p) },
        alternate: matches!(p.action, PromptAction::NewWorkspace)
            .then_some(forms::Button { field: 3, label: "From file..." }),
        cancel: forms::Button { field: count + 1, label: if count == 0 { "Close" } else { "Cancel" } },
    }
}

fn select_workspace(picker: &mut Picker, ws: u32) {
    if !picker.filtered.iter().any(|&i| picker.items[i].action.workspace() == Some(ws)) {
        picker.query.clear();
        picker.cursor = 0;
        picker.refilter();
    }
    picker.select(|i| i.action.workspace() == Some(ws));
}

pub(super) fn form_layout(p: &Prompt, screen: Rect) -> forms::FormLayout {
    let mut message = p
        .workspace_form
        .as_ref()
        .and_then(|f| f.cwd.as_deref())
        .map(|cwd| format!("Starts in {}", crate::paths::shorten_home(cwd)))
        .unwrap_or_default();
    if matches!(p.action, PromptAction::DuplicateWorkspace(_)) {
        message = "Copies the layout.\nPrograms wait for you to start them.".into();
    } else if matches!(p.action, PromptAction::ExportWorkspace(_)) {
        message = "Workspaces are saved automatically.\nExports the layout, without conversations.\nAgents use portable references, without private account names.".into();
    } else if matches!(p.action, PromptAction::ImportWorkspace) {
        message = p.workspace_form.as_ref().and_then(|f| f.preview.as_ref()).map(|p| p.summary()).unwrap_or_default();
    }
    if let Some(form) = &p.workspace_form
        && let Some(account) = form.preview.as_ref().and_then(|p| p.accounts.get(form.account))
    {
        message.push_str(&format!("\nChoose a {} account for {}.", account.agent, account.reference));
    }
    if let Some(error) = &p.error {
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(error);
    }
    if let Some(result) = p.workspace_form.as_ref().and_then(|f| f.result.as_ref()) {
        message = result.clone();
    }
    let mut layout = forms::FormLayout::new(screen, field_count(p), &message, &buttons(p));
    if matches!(p.action, PromptAction::ExportWorkspace(_)) {
        // File paths in errors and results must retain spaces that prose wrapping would collapse.
        layout.lines = message.lines().flat_map(|l| crate::text::fold(l, layout.body.width as usize)).collect();
        layout.max_scroll = layout.lines.len().saturating_sub(layout.body.height as usize);
    }
    layout
}

pub(super) fn draw_form(p: &Prompt, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let form = p.workspace_form.as_ref()?;
    let layout = form_layout(p, screen);
    overlays::frame(layout.area, &p.title, crate::theme::current().accent, buf);
    let mut cursor = None;
    if form.result.is_none() {
        let importing = matches!(p.action, PromptAction::ImportWorkspace);
        if importing {
            cursor = forms::draw_text_field(
                layout.fields[0],
                "File    ",
                &form.file,
                form.file_cursor,
                "Path to a KDL file",
                form.field == 0,
                buf,
            );
            if let Some(preview) = &form.preview
                && let Some(account) = preview.accounts.get(form.account)
            {
                let selected = form.choices[form.account].checked_sub(1).and_then(|n| account.choices.get(n));
                let label = format!(
                    "Account {}/{}    {}",
                    form.account + 1,
                    preview.accounts.len(),
                    selected.map_or("Choose with arrows", |(_, n)| n)
                );
                let area = layout.fields[2];
                buf.set_stringn(area.x, area.y, label, area.width as usize, forms::field_style(form.field == 2));
            }
        }
        let name_field = usize::from(importing);
        let label = if matches!(p.action, PromptAction::ExportWorkspace(_)) { "File    " } else { "Name    " };
        cursor = forms::draw_text_field(
            layout.fields[name_field],
            label,
            &p.value,
            p.cursor,
            "",
            form.field == name_field,
            buf,
        )
        .or(cursor);
    }
    forms::draw_message(&layout, p.scroll, buf);
    let buttons = buttons(p);
    let action = if form.field == buttons.cancel.field {
        buttons.cancel.label
    } else if buttons.alternate.as_ref().is_some_and(|b| b.field == form.field) {
        "From file..."
    } else {
        verb(p)
    };
    forms::draw_buttons(&layout, &buttons, form.field, form.pending, action, buf);
    cursor
}

impl Server {
    pub(super) fn return_to_workspaces(&self, id: ClientId, mode: Mode, state: PickerState) -> Mode {
        match mode {
            Mode::Confirm(mut q) => {
                q.workspace_back = Some(Box::new(state));
                Mode::Confirm(q)
            }
            Mode::Picker(mut p) => {
                let workspace = p.current().and_then(|i| i.action.workspace());
                let message = p.message.clone();
                let undo = p.undo.clone();
                p.restore(state);
                if message.is_some() {
                    p.message = message;
                    p.undo = undo;
                }
                if let Some(ws) = workspace {
                    select_workspace(&mut p, ws);
                }
                Mode::Picker(p)
            }
            Mode::Normal => {
                let mut p = self.workspace_picker(self.clients.get(&id).map_or(0, |c| c.workspace));
                p.restore(state);
                Mode::Picker(p)
            }
            other => other,
        }
    }

    pub(super) fn add_workspace_form(&self, id: ClientId, from: Option<&Picker>) -> Prompt {
        let name = from.map(|p| p.query.trim()).filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(|| {
            (1..).map(|n| format!("workspace-{n}")).find(|n| !self.workspace_name_used(n, None)).unwrap()
        });
        let mut p = Prompt::new("Add workspace", &name, PromptAction::NewWorkspace);
        let form = p.workspace_form.as_mut().unwrap();
        form.cwd = self.workspace_form_cwd(id);
        form.back = from.map(Picker::state);
        p
    }

    fn workspace_form_cwd(&self, id: ClientId) -> Option<PathBuf> {
        self.client_focus(id)
            .and_then(|p| self.panes.get(&p))
            .and_then(|p| p.current_cwd())
            .or_else(|| self.current_ws(id).and_then(|(_, wi)| self.workspaces[wi].cwd.clone()))
            .or_else(|| std::env::current_dir().ok())
    }

    pub(super) fn rename_workspace_form(&self, ws: u32, from: Option<&Picker>) -> Mode {
        let Some(wi) = self.ws_index(ws) else {
            return self.workspace_picker(0).result("Workspace unavailable");
        };
        let mut p = Prompt::new("Rename workspace", &self.workspaces[wi].name, PromptAction::RenameWorkspace(ws));
        p.workspace_form.as_mut().unwrap().back = from.map(Picker::state);
        Mode::Prompt(p)
    }

    pub(super) fn workspace_name_used(&self, name: &str, except: Option<u32>) -> bool {
        self.workspaces.iter().any(|w| Some(w.id) != except && w.name == name)
            || self.unrestored.iter().any(|w| w.name == name)
            || self.definitions().iter().any(|(path, def)| {
                !matches!(self.definition_workspace(path), Ok(Some(_)))
                    && def.as_ref().map_or_else(|_| path.file_stem().is_some_and(|s| s == name), |def| def.name == name)
            })
    }

    pub(super) fn duplicate_workspace_form(&self, ws: u32, from: &Picker) -> Mode {
        let Some(wi) = self.ws_index(ws) else {
            return self.workspace_picker(0).result("Workspace unavailable. Select a workspace to duplicate.");
        };
        let base = format!("{}-copy", self.workspaces[wi].name);
        let name = std::iter::once(base.clone())
            .chain((2..).map(|n| format!("{base}-{n}")))
            .find(|n| !self.workspace_name_used(n, None))
            .unwrap();
        let mut p = Prompt::new("Duplicate workspace", &name, PromptAction::DuplicateWorkspace(ws));
        p.workspace_form.as_mut().unwrap().back = Some(from.state());
        Mode::Prompt(p)
    }

    pub(super) fn workspace_restore_back(&self, p: &mut Prompt, from: Option<&Picker>) {
        p.workspace_form.as_mut().unwrap().back = from.map(Picker::state);
    }

    pub(super) fn on_workspace_form(&mut self, id: ClientId, mut p: Prompt, event: InputEvent) -> Mode {
        let layout = form_layout(&p, self.client_screen(id));
        let buttons = buttons(&p);
        p.scroll = p.scroll.min(layout.max_scroll);
        let importing = matches!(p.action, PromptAction::ImportWorkspace);
        let before = p.value.clone();
        let error = p.error.clone();
        let form = p.workspace_form.as_mut().unwrap();
        let file_before = form.file.clone();
        let old_field = form.field;
        let mut fields = Vec::new();
        if importing {
            fields.push(forms::Field {
                id: 0,
                area: layout.fields[0],
                value: forms::FieldValue::Text { value: &mut form.file, cursor: &mut form.file_cursor },
            });
        }
        if form.result.is_none() {
            fields.push(forms::Field {
                id: usize::from(importing),
                area: layout.fields[usize::from(importing)],
                value: forms::FieldValue::Text { value: &mut p.value, cursor: &mut p.cursor },
            });
        }
        if form.preview.as_ref().is_some_and(|p| !p.accounts.is_empty()) {
            fields.push(forms::Field { id: 2, area: layout.fields[2], value: forms::FieldValue::Choice });
        }
        let outcome = forms::FormState {
            field: &mut form.field,
            scroll: &mut p.scroll,
            error: &mut p.error,
            pending: &mut form.pending,
        }
        .handle(event, &mut fields, &buttons, &layout);
        if form.file != file_before {
            form.preview = None;
            form.existing = None;
            p.scroll = 0;
        } else if p.value == before && p.error.is_none() {
            p.error = error;
        } else if p.value != before {
            p.scroll = 0;
        }
        match outcome {
            forms::FormEvent::Cancel => self.workspace_form_back(id, p, None),
            forms::FormEvent::Submit if matches!(p.action, PromptAction::NewWorkspace) && form.field == 3 => {
                p.action = PromptAction::ImportWorkspace;
                p.value.clear();
                p.cursor = 0;
                p.error = None;
                form.field = 0;
                Mode::Prompt(p)
            }
            forms::FormEvent::Submit => self.submit_workspace_form(id, p),
            forms::FormEvent::Choice { .. } if importing => {
                let i = form.account;
                let choices = &form.preview.as_ref().unwrap().accounts[i].choices;
                form.choices[i] = (form.choices[i] + 1) % (choices.len() + 1);
                p.error = None;
                Mode::Prompt(p)
            }
            _ if importing && old_field == 0 && form.field == 1 && form.preview.is_none() => {
                self.preview_import_form(p)
            }
            _ => Mode::Prompt(p),
        }
    }

    pub(super) fn export_workspace_form(&self, id: ClientId, ws: u32, from: Option<&Picker>) -> Mode {
        let Some(wi) = self.ws_index(ws) else { return self.workspace_picker(0).result("Workspace unavailable") };
        let file = crate::paths::shorten_home(&self.export_file_default(wi));
        let mut p = Prompt::new("Export layout", &file, PromptAction::ExportWorkspace(ws));
        let form = p.workspace_form.as_mut().unwrap();
        form.cwd = self.workspace_form_cwd(id);
        form.back = from.map(Picker::state);
        Mode::Prompt(p)
    }

    fn workspace_form_path(p: &Prompt, value: &str) -> PathBuf {
        let form = p.workspace_form.as_ref().unwrap();
        let path = crate::paths::expand_tilde(value.trim());
        if path.is_relative() { form.cwd.as_ref().map_or(path.clone(), |cwd| cwd.join(&path)) } else { path }
    }

    fn preview_import_form(&self, mut p: Prompt) -> Mode {
        let path = Self::workspace_form_path(&p, &p.workspace_form.as_ref().unwrap().file);
        let form = p.workspace_form.as_mut().unwrap();
        form.preview = None;
        form.existing = self.definition_workspace(&path).ok().flatten();
        match self.preview_import(&path) {
            Ok(preview) => {
                if p.value.is_empty() {
                    p.value = preview.definition.name.clone();
                    p.cursor = p.value.chars().count();
                }
                form.choices = vec![0; preview.accounts.len()];
                form.account = 0;
                form.preview = Some(Box::new(preview));
                form.field = 1;
                p.error = None;
            }
            Err(e) => p.error = Some(format!("{e:#}\nNo workspace was added.")),
        }
        Mode::Prompt(p)
    }

    pub(super) fn exported_layout(&self, mut p: Prompt, path: &std::path::Path) -> Mode {
        p.title = "Layout exported".into();
        p.error = None;
        p.scroll = 0;
        let form = p.workspace_form.as_mut().unwrap();
        form.result = Some(crate::paths::shorten_home(path));
        form.field = 0;
        Mode::Prompt(p)
    }

    fn workspace_form_back(&self, id: ClientId, mut p: Prompt, renamed: Option<u32>) -> Mode {
        let Some(state) = p.workspace_form.as_mut().and_then(|f| f.back.take()) else { return Mode::Normal };
        let mut picker = if state.kind == PickerKind::DeletedWorkspaces {
            self.deleted_workspace_picker()
        } else {
            self.workspace_picker(self.clients.get(&id).map_or(0, |c| c.workspace))
        };
        picker.restore(state);
        if let Some(ws) = renamed {
            select_workspace(&mut picker, ws);
            picker.result("Workspace renamed")
        } else {
            Mode::Picker(picker)
        }
    }

    pub(super) fn submit_workspace_form(&mut self, id: ClientId, mut p: Prompt) -> Mode {
        if p.workspace_form.as_ref().is_some_and(|f| f.result.is_some()) {
            return self.workspace_form_back(id, p, None);
        }
        if let PromptAction::ExportWorkspace(ws) = p.action {
            let result = self
                .ws_index(ws)
                .ok_or_else(|| anyhow::anyhow!("Workspace unavailable. Cancel and choose a workspace."))
                .and_then(|wi| {
                    self.prepare_layout_export(wi, &Self::workspace_form_path(&p, &p.value)).map(|save| (wi, save))
                });
            return match result {
                Ok((wi, save)) => {
                    if let Some(question) = &save.question {
                        return Mode::confirm(
                            question.clone(),
                            super::ui::ConfirmAction::ReplaceWorkspaceFile { prompt: p, save },
                        );
                    }
                    match self.write_definition(wi, &self.workspaces[wi].name.clone(), &save) {
                        Ok(path) => self.exported_layout(p, &path),
                        Err(e) => {
                            p.error = Some(format!("{e:#}"));
                            Mode::Prompt(p)
                        }
                    }
                }
                Err(e) => {
                    p.error = Some(format!("{e:#}"));
                    Mode::Prompt(p)
                }
            };
        }
        if matches!(p.action, PromptAction::ImportWorkspace) {
            let form = p.workspace_form.as_ref().unwrap();
            if let Some(ws) = form.existing {
                return match self.reopen_workspace(ws) {
                    Ok(()) => {
                        self.show_workspace(id, ws);
                        Mode::Normal
                    }
                    Err(e) => {
                        p.error = Some(format!("{e:#}"));
                        Mode::Prompt(p)
                    }
                };
            }
            let Some(preview) = &form.preview else { return self.preview_import_form(p) };
            if !preview.accounts.is_empty() && form.choices[form.account] == 0 {
                p.error = Some(format!(
                    "Choose a {} account for {}. If none is available, add one in Accounts and Retry.",
                    preview.accounts[form.account].agent, preview.accounts[form.account].reference
                ));
                if preview.accounts[form.account].choices.is_empty() {
                    p.workspace_form.as_mut().unwrap().preview = None;
                }
                return Mode::Prompt(p);
            }
            if form.account + 1 < preview.accounts.len() {
                let form = p.workspace_form.as_mut().unwrap();
                form.account += 1;
                form.field = 2;
                return Mode::Prompt(p);
            }
            let choices = preview
                .accounts
                .iter()
                .enumerate()
                .filter_map(|(i, a)| {
                    Some((a.reference.clone(), a.choices.get(form.choices[i].checked_sub(1)?)?.0.clone()))
                })
                .collect::<Vec<_>>();
            return match self.import_layout(&preview.path, p.value.trim(), &preview.revision, &choices) {
                Ok(ws) => {
                    self.show_workspace(id, ws);
                    self.toast(id, "Workspace added. Commands wait for you to start them.");
                    Mode::Normal
                }
                Err(e) => {
                    p.error = Some(format!("{e:#}"));
                    if crate::paths::FileRevision::read(&preview.path).ok().as_ref() != Some(&preview.revision) {
                        p.workspace_form.as_mut().unwrap().preview = None;
                    }
                    Mode::Prompt(p)
                }
            };
        }
        let name = p.value.trim();
        if let PromptAction::DuplicateWorkspace(source) = p.action {
            let validation = if name.is_empty() {
                Some("Type a name.")
            } else if self.workspace_name_used(name, None) {
                Some("This name is already used.")
            } else {
                None
            };
            p.workspace_form.as_mut().unwrap().invalid_name = validation.is_some();
            let result = match validation {
                Some(error) => Err(error.into()),
                None => self.duplicate_workspace(source, name),
            };
            return match result {
                Ok(ws) => {
                    self.refresh_workspace_pickers();
                    self.mark_dirty();
                    self.toast(id, "Workspace duplicated");
                    let mut picker = self.workspace_picker(self.clients.get(&id).map_or(0, |c| c.workspace));
                    if let Some(back) = p.workspace_form.as_mut().and_then(|f| f.back.take()) {
                        picker.restore(back);
                    }
                    select_workspace(&mut picker, ws);
                    picker.result("Workspace duplicated · Enter Open")
                }
                Err(error) => {
                    p.error = Some(error);
                    p.scroll = 0;
                    Mode::Prompt(p)
                }
            };
        }
        if let PromptAction::RestoreWorkspace(identity) = &p.action {
            let result = if name.is_empty() {
                Err("Type a name.".into())
            } else {
                self.restore_deleted_workspace(identity, name)
            };
            return match result {
                Ok(ws) => {
                    self.refresh_workspace_pickers();
                    self.mark_dirty();
                    self.toast(id, "Workspace restored");
                    let mut picker = self.workspace_picker(0);
                    picker.select(|i| i.action.workspace() == Some(ws));
                    picker.result("Workspace restored · Enter Open")
                }
                Err(error) => {
                    p.error = Some(error);
                    p.scroll = 0;
                    Mode::Prompt(p)
                }
            };
        }
        let target = match p.action {
            PromptAction::RenameWorkspace(ws) => Some(ws),
            PromptAction::NewWorkspace => None,
            _ => unreachable!("a workspace name form"),
        };
        let validation = if name.is_empty() {
            Some("Type a name.")
        } else if self.workspace_name_used(name, target) {
            Some("This name is already used.")
        } else {
            None
        };
        p.workspace_form.as_mut().unwrap().invalid_name = validation.is_some();
        let result = if let Some(error) = validation {
            Err(error.into())
        } else if let Some(ws) = target {
            self.rename_workspace(ws, name)
        } else {
            let cwd = p.workspace_form.as_ref().and_then(|f| f.cwd.clone());
            self.add_workspace(name, cwd).map(|ws| self.show_workspace(id, ws))
        };
        match result {
            Ok(()) => {
                self.refresh_workspace_pickers();
                self.mark_dirty();
                self.toast(id, if target.is_some() { "Workspace renamed" } else { "Workspace added" });
                if target.is_some() { self.workspace_form_back(id, p, target) } else { Mode::Normal }
            }
            Err(error) => {
                p.error = Some(error);
                p.scroll = 0;
                Mode::Prompt(p)
            }
        }
    }

    fn rename_workspace(&mut self, ws: u32, name: &str) -> Result<(), String> {
        let wi =
            self.ws_index(ws).ok_or("This workspace is no longer available. Cancel and choose another workspace.")?;
        let old = std::mem::replace(&mut self.workspaces[wi].name, name.to_string());
        if let Err(e) = self.save_now() {
            self.workspaces[wi].name = old.clone();
            // A timed-out writer may still finish. Confirm the original name is written after it.
            if let Err(rollback) = self.save_now() {
                self.mark_persist();
                return Err(format!(
                    "Couldn't save the name or confirm its rollback. The saved session may still use {name}.\nFix the save error and retry.\n{e}\n{rollback}"
                ));
            }
            return Err(format!("Couldn't save the name. Still named {old}.\n{e}"));
        }
        Ok(())
    }

    fn add_workspace(&mut self, name: &str, cwd: Option<PathBuf>) -> Result<u32, String> {
        if let Some(dir) = &cwd
            && !dir.is_dir()
        {
            return Err(format!(
                "The starting folder {} is no longer available. Restore it and retry, or cancel and open Add workspace from another folder.",
                crate::paths::shorten_home(dir)
            ));
        }
        let panes = self.panes.keys().copied().collect();
        let workspaces: Vec<_> = self.workspaces.iter().map(|w| w.id).collect();
        let ws = self.new_workspace(name.into(), None);
        let wi = self.ws_index(ws).unwrap();
        self.workspaces[wi].cwd = cwd.clone();
        if let Err(e) = self.spawn_pane_in(ws, None, PaneSpec { cwd, ..Default::default() }, Placement::Tab, None) {
            self.rollback(&panes, &workspaces);
            return Err(format!("Couldn't start the shell. No workspace was added.\n{e:#}"));
        }
        if let Err(e) = self.save_now() {
            self.rollback(&panes, &workspaces);
            if let Err(rollback) = self.save_now() {
                self.mark_persist();
                return Err(format!(
                    "Couldn't save the workspace or confirm its rollback. The saved session may still contain {name}.\nFix the save error and retry.\n{e}\n{rollback}"
                ));
            }
            return Err(format!("Couldn't save the workspace. No workspace was added.\n{e}"));
        }
        Ok(ws)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, KeySpec, MouseButton, MouseEvent, MouseKind};
    use crate::server::ui::PickAction;

    fn key(spec: &str) -> InputEvent {
        let k = KeySpec::parse(spec).unwrap();
        InputEvent::Key(Key { code: k.code, mods: k.mods, raw: Vec::new() })
    }

    fn text(buf: &Buffer) -> String {
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    fn confirm(server: &mut Server, client: ClientId, q: super::super::ui::Question, bytes: &[u8]) -> Mode {
        server.clients.get_mut(&client).unwrap().mode = Mode::Confirm(q);
        server.client_input(client, bytes);
        std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal)
    }

    #[test]
    fn import_form_previews_keeps_errors_and_offers_existing_workspace_at_both_sizes() {
        for size in [(80, 24), (124, 36)] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            let path = std::env::temp_dir().join(format!("kraal-w10-form-{}-{}.kdl", std::process::id(), size.0));
            std::fs::write(&path, "broken {").unwrap();
            let mut p = server.add_workspace_form(client, None);
            let layout = form_layout(&p, server.client_screen(client));
            let area = layout.alternate.unwrap();
            let event = InputEvent::Mouse(MouseEvent {
                kind: MouseKind::Press(MouseButton::Left),
                col: area.x,
                row: area.y,
                mods: Default::default(),
            });
            let Mode::Prompt(next) = server.on_workspace_form(client, p, event) else { panic!() };
            p = next;
            assert!(matches!(p.action, PromptAction::ImportWorkspace));
            p.workspace_form.as_mut().unwrap().file = path.display().to_string();
            p.value = "personal".into();
            let Mode::Prompt(mut p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert!(p.error.as_ref().unwrap().contains("No workspace was added"));
            assert_eq!(p.value, "personal");
            std::fs::write(&path, "layout { pane command=\"true\"; }").unwrap();
            let Mode::Prompt(next) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            p = next;
            assert!(p.workspace_form.as_ref().unwrap().preview.is_some());
            assert!(server.panes.is_empty());
            let mut buf = Buffer::empty(server.client_screen(client));
            draw_form(&p, buf.area, &mut buf);
            for expected in ["File", "Name    personal", "1 tab · 1 pane", "Commands wait", "Enter Add"] {
                assert!(text(&buf).contains(expected), "{}", text(&buf));
            }
            super::super::persistence::tests::writer(&mut server, vec![Ok(())]);
            assert!(matches!(server.on_workspace_form(client, p, key("enter")), Mode::Normal));
            assert!(server.panes.values().all(|p| p.suspended));
            let mut p = Prompt::new("Add workspace", "", PromptAction::ImportWorkspace);
            p.workspace_form.as_mut().unwrap().file = path.display().to_string();
            let Mode::Prompt(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!(verb(&p), "Open");
            assert!(p.error.as_ref().unwrap().contains("already used by personal"));
            assert!(matches!(server.on_workspace_form(client, p, key("enter")), Mode::Normal));
            assert_eq!(server.workspaces.len(), 2);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn missing_accounts_are_chosen_explicitly_one_at_a_time_without_overflowing_the_form() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let path = std::env::temp_dir().join(format!("kraal-w10-many-accounts-{}.kdl", std::process::id()));
        let panes = (0..30).map(|n| format!("pane agent=\"claude:missing-{n}\"; ")).collect::<String>();
        std::fs::write(&path, format!("layout {{ {panes} }}")).unwrap();
        let account: crate::accounts::Account = serde_json::from_value(serde_json::json!({
            "id": "choice-id", "name": "chosen", "agent": "claude", "home": "/tmp/kraal-w10-choice"
        }))
        .unwrap();
        server.accounts.add(account).unwrap();
        let mut p = Prompt::new("Add workspace", "chosen-name", PromptAction::ImportWorkspace);
        p.workspace_form.as_mut().unwrap().file = path.display().to_string();
        let Mode::Prompt(next) = server.preview_import_form(p) else { panic!() };
        p = next;
        for n in 0..30 {
            for (cols, rows) in [(80, 24), (124, 36)] {
                let area = Rect::new(0, 0, cols, rows);
                let layout = form_layout(&p, area);
                assert!(layout.area.bottom() < rows);
                assert_eq!(layout.fields.len(), 3);
                let mut buf = Buffer::empty(area);
                draw_form(&p, area, &mut buf);
                assert!(text(&buf).contains(&format!("claude:missing-{n}")));
            }
            assert_eq!(p.workspace_form.as_ref().unwrap().choices[n], 0);
            p.workspace_form.as_mut().unwrap().field = 2;
            let Mode::Prompt(next) = server.on_workspace_form(client, p, key("right")) else { panic!() };
            p = next;
            assert_eq!(p.workspace_form.as_ref().unwrap().choices[n], 1);
            if n < 29 {
                assert_eq!(verb(&p), "Next");
                let Mode::Prompt(next) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
                p = next;
            }
        }
        assert_eq!(verb(&p), "Add");
        assert!(server.panes.is_empty());
        assert!(matches!(server.on_workspace_form(client, p, key("esc")), Mode::Normal));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn export_confirmation_defaults_to_cancel_and_retries_with_the_path_intact() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let ws = server.workspaces[0].id;
        let dir = std::env::temp_dir().join(format!("kraal-w10-export-form-{}", std::process::id()));
        let files = dir.join("files");
        std::fs::create_dir_all(&files).unwrap();
        let alias = dir.join("alias");
        std::os::unix::fs::symlink(&files, &alias).unwrap();
        let path = alias.join("work  layout.kdl");
        let lock = files.join(".work  layout.kdl.lock");
        std::fs::write(&path, "// manual\nlayout { pane; }").unwrap();
        super::super::persistence::tests::writer(&mut server, vec![Ok(())]);
        let mut list = server.workspace_picker(ws);
        list.query = "test".into();
        list.refilter();
        let Mode::Prompt(mut p) = server.export_workspace_form(client, ws, Some(&list)) else { panic!() };
        p.value = path.display().to_string();
        p.cursor = 4;
        let Mode::Confirm(q) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        assert!(!q.yes);
        assert!(q.message.contains("Comments and unsupported KDL"));
        let Mode::Prompt(p) = confirm(&mut server, client, q, b"\r") else { panic!() };
        assert_eq!(p.cursor, 4);
        let Mode::Confirm(q) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        std::fs::write(&path, "second edit").unwrap();
        let Mode::Prompt(p) = confirm(&mut server, client, q, b"y") else { panic!() };
        assert!(p.error.as_ref().unwrap().contains("changed while saving"));
        for size in [(80, 24), (124, 36)] {
            let mut buf = Buffer::empty(Rect::new(0, 0, size.0, size.1));
            draw_form(&p, buf.area, &mut buf);
            assert!(text(&buf).contains("Enter Retry"));
            assert!(text(&buf).contains("Workspaces are saved automatically."));
            assert!(form_layout(&p, buf.area).lines.join("").contains(p.error.as_ref().unwrap()));
        }
        let Mode::Confirm(q) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        std::fs::create_dir(&lock).unwrap();
        let Mode::Prompt(p) = confirm(&mut server, client, q, b"y") else { panic!() };
        assert!(p.error.is_some());
        assert_eq!(p.value, path.display().to_string());
        std::fs::remove_dir(&lock).unwrap();
        let Mode::Confirm(q) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        let Mode::Prompt(p) = confirm(&mut server, client, q, b"y") else { panic!() };
        assert_eq!(p.title, "Layout exported");
        assert_eq!(
            p.workspace_form.as_ref().unwrap().result.as_deref(),
            Some(path.canonicalize().unwrap().to_str().unwrap())
        );
        let Mode::Picker(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        assert_eq!(p.query, "test");
        assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), "second edit");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relative_export_paths_use_the_directory_captured_when_the_form_opens() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let ws = server.workspaces[0].id;
        let dir = std::env::temp_dir().join(format!("kraal-export-relative-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let pane = server
            .spawn_pane_suspended(
                None,
                PaneSpec { cwd: Some(dir.clone()), command: vec!["true".into()], ..Default::default() },
                "start_suspended",
            )
            .unwrap();
        server.workspaces[0].tabs[0].root = Some(crate::layout::Node::Leaf(pane));
        server.workspaces[0].tabs[0].focus = Some(pane);
        super::super::persistence::tests::writer(&mut server, vec![]);
        for value in ["copy.kdl", "./copy.kdl"] {
            server.panes.get_mut(&pane).unwrap().reported_cwd = Some(dir.clone());
            let Mode::Prompt(mut prompt) = server.export_workspace_form(client, ws, None) else { panic!() };
            assert_eq!(prompt.workspace_form.as_ref().unwrap().cwd.as_ref(), Some(&dir));
            server.panes.get_mut(&pane).unwrap().reported_cwd = Some(dir.join("elsewhere"));
            prompt.value = value.into();
            let Mode::Prompt(result) = server.submit_workspace_form(client, prompt) else { panic!() };
            assert_eq!(result.title, "Layout exported", "{:?}", result.error);
            let path = dir.join("copy.kdl");
            assert!(path.is_file());
            assert_eq!(crate::workspace::load_file(&path).unwrap().panes()[0].command.as_deref(), Some("true"));
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_result_keeps_the_full_path_readable_and_returns_to_the_list() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let ws = server.workspaces[0].id;
        let list = server.workspace_picker(ws);
        let path = PathBuf::from(format!("/{}shared.kdl", "long folder 日本/".repeat(200)));
        for (cols, rows) in [(80, 24), (124, 36)] {
            let Mode::Prompt(p) = server.export_workspace_form(client, ws, Some(&list)) else { panic!() };
            let Mode::Prompt(mut p) = server.exported_layout(p, &path) else { panic!() };
            let area = Rect::new(0, 0, cols, rows);
            let layout = form_layout(&p, area);
            assert!(layout.fields.is_empty());
            assert!(layout.max_scroll > 0);
            assert_eq!(layout.lines.join(""), path.to_str().unwrap());
            p.scroll = layout.max_scroll;
            let mut buf = Buffer::empty(area);
            assert!(draw_form(&p, area, &mut buf).is_none());
            for expected in ["Layout exported", "shared.kdl", "Enter Done", "Esc Close"] {
                assert!(text(&buf).contains(expected), "{}", text(&buf));
            }
            let Mode::Picker(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!(p.current().unwrap().action.workspace(), Some(ws));
        }
    }

    #[test]
    fn duplicate_form_proposes_a_free_name_and_cancel_keeps_search_and_selection() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let ws = server.workspaces[0].id;
        server.new_workspace("test-copy".into(), None);
        let mut kept = crate::persist::snapshot(&server).workspaces.remove(1);
        kept.closed = true;
        server.workspaces[1].kept = Some(kept);
        server.new_workspace("test-copy-2".into(), None);
        let failed = crate::persist::snapshot(&server).workspaces.remove(2);
        server.workspaces.pop();
        server.unrestored.push(failed);
        let dir = std::env::temp_dir().join(format!("kraal-duplicate-name-{}", crate::accounts::new_uuid()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("test-copy-3.kdl"), "layout { pane; }\n").unwrap();
        server.config.workspace_dirs = vec![dir.display().to_string()];
        let mut list = server.workspace_picker(ws);
        list.query = "test".into();
        list.cursor = 2;
        list.refilter();
        for size in [(80, 24), (124, 36)] {
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            let Mode::Prompt(p) = server.duplicate_workspace_form(ws, &list) else { panic!() };
            assert_eq!(p.value, "test-copy-4");
            let mut buf = Buffer::empty(server.client_screen(client));
            draw_form(&p, buf.area, &mut buf);
            for expected in [
                "Duplicate workspace",
                "Name    test-copy-4",
                "Copies the layout.",
                "Programs wait for you to start them.",
                "Enter Duplicate",
                "Esc Cancel",
            ] {
                assert!(text(&buf).contains(expected));
            }
            let Mode::Picker(back) = server.on_workspace_form(client, p, key("esc")) else { panic!() };
            assert_eq!((back.query.as_str(), back.cursor), ("test", 2));
            assert_eq!(back.current().unwrap().action.workspace(), Some(ws));
            assert_eq!(server.workspaces.len(), 2);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn duplicate_form_keeps_errors_and_input_then_selects_the_durable_closed_copy() {
        for size in [(80, 24), (124, 36)] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            let ws = server.workspaces[0].id;
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            let saved = super::super::persistence::tests::writer(&mut server, vec![Err("disk full".into()), Ok(())]);
            let mut list = server.workspace_picker(ws);
            list.query = "test".into();
            list.refilter();
            let Mode::Prompt(mut p) = server.duplicate_workspace_form(ws, &list) else { panic!() };
            for value in ["", "test"] {
                p.value = value.into();
                let Mode::Prompt(next) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
                assert!(next.error.is_some());
                assert_eq!(verb(&next), "Duplicate");
                p = next;
            }
            p.value = "independent".into();
            p.cursor = 3;
            let Mode::Prompt(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!((p.value.as_str(), p.cursor), ("independent", 3));
            assert_eq!(verb(&p), "Retry");
            assert!(p.error.as_ref().unwrap().contains("No workspace was added"));
            let mut buf = Buffer::empty(server.client_screen(client));
            draw_form(&p, buf.area, &mut buf);
            assert!(text(&buf).contains("disk full"));
            assert!(text(&buf).contains("Programs wait for you to start them."));
            assert_eq!(server.workspaces.len(), 1);
            let button = form_layout(&p, server.client_screen(client)).primary;
            let Mode::Picker(p) = server.on_workspace_form(
                client,
                p,
                InputEvent::Mouse(MouseEvent {
                    kind: MouseKind::Press(MouseButton::Left),
                    col: button.x,
                    row: button.y,
                    mods: Default::default(),
                }),
            ) else {
                panic!()
            };
            assert_eq!(p.message.as_deref(), Some("Workspace duplicated · Enter Open"));
            assert!(p.query.is_empty());
            let copy = &server.workspaces[1];
            assert_eq!(p.current().unwrap().action.workspace(), Some(copy.id));
            assert!(copy.closed());
            assert_eq!(server.clients[&client].workspace, ws);
            assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"][1]["name"], "independent");
        }
    }

    #[test]
    fn screen_name_validation_keeps_add_and_rename_buttons() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            let ws = server.workspaces[0].id;
            server.new_workspace("taken".into(), None);
            for name in ["taken", " "] {
                for rename in [false, true] {
                    let mut p = if rename {
                        let Mode::Prompt(p) = server.rename_workspace_form(ws, None) else { panic!() };
                        p
                    } else {
                        server.add_workspace_form(client, None)
                    };
                    p.value = name.into();
                    let Mode::Prompt(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
                    let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
                    draw_form(&p, buf.area, &mut buf);
                    let shown: Vec<String> =
                        buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
                    let verb = if rename { "Rename" } else { "Add" };
                    assert!(shown.iter().any(|row| row.contains(p.error.as_ref().unwrap())));
                    assert!(shown.iter().any(|row| row.contains(&format!("[> {verb}]"))), "{shown:?}");
                    assert!(
                        shown.iter().any(|row| row.contains(&format!("Enter {verb} · Tab Next field · Esc Cancel")))
                    );
                    assert!(!shown.iter().any(|row| row.contains("Retry")));
                }
            }
        }
    }

    #[test]
    fn screen_add_keeps_the_starting_folder_beside_a_name_error() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        server.workspaces[0].cwd = Some("/work/api".into());
        let mut p = server.add_workspace_form(client, None);
        p.value = "test".into();
        let Mode::Prompt(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        for (cols, rows) in [(80, 24), (124, 36)] {
            let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
            draw_form(&p, buf.area, &mut buf);
            let shown: Vec<String> =
                buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
            for expected in ["Name    test", "Starts in /work/api", "This name is already used."] {
                assert!(shown.iter().any(|row| row.contains(expected)), "{shown:?}");
            }
        }
    }

    #[test]
    fn add_announces_the_inherited_folder_and_cancel_restores_the_search() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        server.workspaces[0].cwd = Some("/work/api".into());
        let mut list = server.workspace_picker(server.workspaces[0].id);
        list.query = "billing".into();
        list.cursor = 3;
        list.refilter();
        for size in [(80, 24), (124, 36)] {
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            let p = server.add_workspace_form(client, Some(&list));
            assert_eq!(p.value, "billing");
            let mut buf = Buffer::empty(server.client_screen(client));
            draw_form(&p, buf.area, &mut buf);
            let shown = text(&buf);
            for expected in ["Add workspace", "Name    billing", "Starts in /work/api", "Enter Add", "Esc Cancel"] {
                assert!(shown.contains(expected), "{shown}");
            }
            let Mode::Picker(back) = server.on_workspace_form(client, p, key("esc")) else { panic!() };
            assert_eq!((back.query.as_str(), back.cursor), ("billing", 3));
            assert!(matches!(back.current().unwrap().action, PickAction::AddWorkspace));
            assert_eq!(server.workspaces.len(), 1);
        }
        let p = server.add_workspace_form(client, None);
        assert_eq!(p.value, "workspace-1");
        server.new_workspace(p.value, None);
        assert_eq!(server.add_workspace_form(client, None).value, "workspace-2");
    }

    #[test]
    fn rename_is_durable_for_open_and_closed_workspaces_without_touching_a_source() {
        for closed in [false, true] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            let ws = server.workspaces[0].id;
            let path = std::env::temp_dir().join(format!("kraal-w6-rename-{}-{closed}.kdl", std::process::id()));
            std::fs::write(&path, "// keep my comments\nlayout { pane; }\n").unwrap();
            server.workspaces[0].set_definition(Some(&path));
            let source = server.workspaces[0].definition.clone();
            let identity = server.workspaces[0].stable_id.clone();
            if closed {
                let mut snap = crate::persist::snapshot(&server).workspaces.remove(0);
                snap.closed = true;
                server.workspaces[0].kept = Some(snap);
            }
            let saved = super::super::persistence::tests::writer(&mut server, vec![Err("disk full".into()), Ok(())]);
            let mut list = server.workspace_picker(ws);
            list.query = "test".into();
            list.cursor = 2;
            list.refilter();
            let Mode::Prompt(mut p) = server.rename_workspace_form(ws, Some(&list)) else { panic!() };
            p.value = "test/review".into();
            p.cursor = 5;
            let Mode::Prompt(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!((p.value.as_str(), p.cursor), ("test/review", 5));
            assert!(p.error.as_ref().unwrap().contains("Still named test."));
            for (cols, rows) in [(80, 24), (124, 36)] {
                let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
                draw_form(&p, buf.area, &mut buf);
                let shown: Vec<String> =
                    buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
                assert!(shown.iter().any(|row| row.contains("[> Retry]")));
                assert!(shown.iter().any(|row| row.contains("Enter Retry")));
            }
            assert_eq!(server.workspaces[0].name, "test");
            assert!(server.clients[&client].toast.is_none());
            let Mode::Picker(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!(p.query, "test");
            assert_eq!(p.current().unwrap().label, "test/review");
            assert_eq!(p.message.as_deref(), Some("Workspace renamed"));
            assert_eq!(server.workspaces[0].stable_id, identity);
            assert_eq!(server.workspaces[0].definition, source);
            assert_eq!(server.workspaces[0].closed(), closed);
            assert!(server.panes.is_empty());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "// keep my comments\nlayout { pane; }\n");
            assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"][0]["name"], "test/review");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn rename_clears_a_filter_that_hides_the_new_name_and_keeps_the_workspace_selected() {
        for closed in [false, true] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            let ws = server.workspaces[0].id;
            server.workspaces[0].name = "zebra".into();
            if closed {
                let mut snap = crate::persist::snapshot(&server).workspaces.remove(0);
                snap.closed = true;
                server.workspaces[0].kept = Some(snap);
            }
            server.new_workspace("other".into(), None);
            let saved = super::super::persistence::tests::writer(&mut server, vec![Ok(())]);
            let mut list = server.workspace_picker(ws);
            list.query = "zebra".into();
            list.cursor = 2;
            list.refilter();
            let Mode::Prompt(mut p) = server.rename_workspace_form(ws, Some(&list)) else { panic!() };
            p.value = "renamed".into();
            let Mode::Picker(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!((p.query.as_str(), p.cursor), ("", 0));
            assert_eq!(p.current().unwrap().action.workspace(), Some(ws));
            assert_eq!(p.current().unwrap().label, "renamed");
            assert_eq!(p.message.as_deref(), Some("Workspace renamed"));
            assert_eq!(server.workspaces[0].closed(), closed);
            assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"][0]["name"], "renamed");
        }
    }

    #[test]
    fn add_keeps_an_error_if_the_announced_folder_disappears_before_submit() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let ws = server.workspaces[0].id;
        let dir = std::env::temp_dir().join(format!("kraal-add-folder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _batch = crate::pane::batch();
        let pane = server.spawn_pane_in(ws, None, PaneSpec::default(), Placement::Auto, None).unwrap();
        server.panes.get_mut(&pane).unwrap().reported_cwd = Some(dir.clone());
        let saved = super::super::persistence::tests::writer(&mut server, vec![Ok(())]);
        let mut p = server.add_workspace_form(client, None);
        p.value = "new workspace".into();
        p.cursor = 3;
        let mut buf = Buffer::empty(server.client_screen(client));
        draw_form(&p, buf.area, &mut buf);
        // A long temporary directory (as on macOS) wraps across message rows.
        let body = form_layout(&p, buf.area).body;
        let shown: Vec<_> = (body.y..body.bottom())
            .map(|y| {
                let row: String = (body.x..body.right()).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect();
        let message = format!("Starts in {}", crate::paths::shorten_home(&dir));
        assert_eq!(shown, crate::text::wrap(&message, body.width as usize));
        std::fs::remove_dir(&dir).unwrap();
        let Mode::Prompt(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        assert_eq!((p.value.as_str(), p.cursor), ("new workspace", 3));
        let error = p.error.clone().unwrap();
        assert!(error.contains(&format!("The starting folder {} is no longer available", dir.display())));
        assert!(error.contains("Restore it and retry, or cancel"));
        assert_eq!(p.workspace_form.as_ref().unwrap().cwd.as_ref(), Some(&dir));
        assert_eq!((server.workspaces.len(), server.panes.len()), (1, 1));
        assert_eq!(server.clients[&client].workspace, ws);
        assert!(server.clients[&client].toast.is_none());
        assert!(saved.lock().unwrap().is_empty());
        let Mode::Prompt(p) = server.on_workspace_form(client, p, key("left")) else { panic!() };
        assert_eq!(p.error.as_deref(), Some(error.as_str()));
        std::fs::create_dir(&dir).unwrap();
        assert!(matches!(server.on_workspace_form(client, p, key("enter")), Mode::Normal));
        let added = server.workspaces.last().unwrap();
        assert_eq!(added.cwd.as_ref(), Some(&dir));
        assert_eq!(server.panes[&added.panes()[0]].spec.cwd.as_ref(), Some(&dir));
        assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"][1]["cwd"], dir.to_str().unwrap());
        std::fs::remove_dir(dir).unwrap();
    }

    fn rollback_after_a_late_write(rename: bool, empty: bool) {
        use super::super::persistence::Persist;
        for rollback_fails in [false, true] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            let ws = server.workspaces[0].id;
            if empty {
                server.workspaces.clear();
            }
            server.session = format!("workspace-rollback-{rename}-{empty}-{rollback_fails}");
            server.config.save_scrollback = false;
            let before = crate::persist::snapshot(&server);
            crate::persist::save(&server.session, &before).unwrap();
            let (writer, jobs) = crossbeam_channel::unbounded();
            server.persister = writer;
            let thread = std::thread::spawn(move || {
                let mut saves = 0;
                let mut flushes = 0;
                let mut latest: Option<(String, crate::persist::SessionSnapshot)> = None;
                let mut delayed = None;
                while let Ok(job) = jobs.recv() {
                    match job {
                        Persist::Save(session, snap) => {
                            saves += 1;
                            if saves == 2 {
                                // Publish the rejected change only after its timeout queued the rollback.
                                let (session, snap) = latest.take().unwrap();
                                crate::persist::save(&session, &snap).unwrap();
                            }
                            latest = Some((session, snap));
                        }
                        Persist::Flush(done) => {
                            flushes += 1;
                            if flushes == 1 {
                                delayed = Some(done);
                                continue;
                            }
                            let answer = if flushes == 2 && rollback_fails {
                                Err("rollback disk full".into())
                            } else {
                                let (session, snap) = latest.as_ref().unwrap();
                                crate::persist::save(session, snap).map_err(|e| e.to_string())
                            };
                            let _ = done.send(answer);
                            delayed.take();
                        }
                        _ => {}
                    }
                }
            });
            let _batch = crate::pane::batch();
            let mut p = if rename {
                let Mode::Prompt(p) = server.rename_workspace_form(ws, None) else { panic!() };
                p
            } else {
                server.add_workspace_form(client, None)
            };
            p.value = "rejected".into();
            p.cursor = 3;
            let Mode::Prompt(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            let error = p.error.as_ref().unwrap();
            assert!(error.contains("writer did not finish in time"), "{error}");
            assert_eq!((p.value.as_str(), p.cursor), ("rejected", 3));
            assert_eq!(server.workspaces.len(), usize::from(!empty));
            assert!(server.workspaces.iter().all(|w| w.name == "test"));
            assert!(server.panes.is_empty());
            assert_eq!(server.clients[&client].workspace, ws);
            assert!(server.clients[&client].toast.is_none());
            let disk = crate::persist::load(&server.session).unwrap().unwrap();
            if rollback_fails {
                assert!(error.contains("or confirm its rollback"), "{error}");
                assert!(error.contains("The saved session may still"), "{error}");
                assert!(error.contains("Fix the save error and retry."), "{error}");
                assert!(error.contains("rollback disk full"), "{error}");
                assert!(!error.contains("Still named") && !error.contains("No workspace was added"), "{error}");
                assert_eq!(disk.workspaces.last().unwrap().name, "rejected");
                assert!(server.persist_dirty);
            } else {
                assert!(error.contains(if rename { "Still named test." } else { "No workspace was added." }));
                assert_eq!(serde_json::to_value(disk).unwrap(), serde_json::to_value(before).unwrap());
            }
            assert!(matches!(server.on_workspace_form(client, p, key("enter")), Mode::Normal));
            let disk = crate::persist::load(&server.session).unwrap().unwrap();
            assert_eq!(disk.workspaces.len(), usize::from(!empty) + usize::from(!rename));
            assert_eq!(disk.workspaces.last().unwrap().name, "rejected");
            drop(server);
            thread.join().unwrap();
        }
    }

    #[test]
    fn rename_confirms_rollback_after_a_late_write_and_reports_failed_rollback() {
        rollback_after_a_late_write(true, false);
    }

    #[test]
    fn add_confirms_rollback_after_a_late_write_and_reports_failed_rollback() {
        for empty in [false, true] {
            rollback_after_a_late_write(false, empty);
        }
    }

    #[test]
    fn names_are_checked_against_closed_workspaces_files_and_unrestored_records() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let ws = server.workspaces[0].id;
        let mut snap = crate::persist::snapshot(&server).workspaces.remove(0);
        snap.name = "unrestored".into();
        server.unrestored.push(snap.clone());
        let closed = server.new_workspace("closed".into(), None);
        snap.closed = true;
        server.workspaces[1].kept = Some(snap);
        let dir = std::env::temp_dir().join(format!("kraal-w6-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.kdl"), "layout { pane; }").unwrap();
        std::fs::write(dir.join("invalid.kdl"), "{").unwrap();
        server.config.workspace_dirs.push(dir.display().to_string());
        for name in ["test", "closed", "unrestored", "file", "invalid"] {
            for target in [None, Some(ws)] {
                if target.is_some() && name == "test" {
                    continue;
                }
                let mut p = match target {
                    None => server.add_workspace_form(client, None),
                    Some(ws) => {
                        let Mode::Prompt(p) = server.rename_workspace_form(ws, None) else { panic!() };
                        p
                    }
                };
                p.value = name.into();
                let Mode::Prompt(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
                assert_eq!(p.error.as_deref(), Some("This name is already used."));
                assert_eq!(server.workspaces.len(), 2);
                assert!(server.panes.is_empty());
                assert_eq!(server.workspaces[1].id, closed);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn add_rolls_back_a_failed_save_and_retry_adds_exactly_one_workspace() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let before = server.workspaces[0].id;
        let _batch = crate::pane::batch();
        let saved = super::super::persistence::tests::writer(&mut server, vec![Err("disk full".into()), Ok(())]);
        let p = server.add_workspace_form(client, None);
        let Mode::Prompt(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        assert!(p.error.as_ref().unwrap().contains("Couldn't save the workspace. No workspace was added."));
        for (cols, rows) in [(80, 24), (124, 36)] {
            let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
            draw_form(&p, buf.area, &mut buf);
            let shown: Vec<String> =
                buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
            assert!(shown.iter().any(|row| row.contains("[> Retry]")));
            assert!(shown.iter().any(|row| row.contains("Enter Retry")));
            assert!(shown.iter().any(|row| row.contains("Starts in")));
        }
        assert_eq!(server.workspaces.len(), 1);
        assert!(server.panes.is_empty());
        assert_eq!(server.clients[&client].workspace, before);
        assert!(server.clients[&client].toast.is_none());
        assert!(matches!(server.on_workspace_form(client, p, key("enter")), Mode::Normal));
        assert_eq!(server.workspaces.len(), 2);
        assert_eq!(server.panes.len(), 1);
        let added = server.workspaces.last().unwrap();
        assert_eq!(server.clients[&client].workspace, added.id);
        assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn form_errors_scroll_and_buttons_work_with_keyboard_and_mouse_at_both_sizes() {
        for size in [(80, 24), (124, 36)] {
            let (tx, _rx) = super::super::queue::channel(32);
            let mut server = super::super::tests::server(tx);
            let (client, _peer) = super::super::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            let ws = server.workspaces[0].id;
            let Mode::Prompt(mut p) = server.rename_workspace_form(ws, None) else { panic!() };
            p.value = "café 日".into();
            p.cursor = 4;
            p.error = Some(format!("{}Final detail", "Long diagnostic with 日本語. ".repeat(200)));
            let layout = form_layout(&p, server.client_screen(client));
            assert!(layout.max_scroll > 0);
            for _ in 0..100 {
                let Mode::Prompt(next) = server.on_workspace_form(client, p, key("pagedown")) else { panic!() };
                p = next;
            }
            assert_eq!(p.scroll, layout.max_scroll);
            let Mode::Prompt(next) = server.on_workspace_form(client, p, key("left")) else { panic!() };
            p = next;
            assert!(p.error.is_some());
            let mut buf = Buffer::empty(server.client_screen(client));
            let cursor = draw_form(&p, buf.area, &mut buf).unwrap();
            assert!(layout.fields[0].contains((cursor.x, cursor.y).into()));
            let shown = text(&buf);
            for expected in ["café", "Final detail", "Enter Retry", "PgUp/PgDn Scroll", "Esc Cancel"] {
                assert!(shown.contains(expected), "{shown}");
            }
            let Mode::Prompt(next) = server.on_workspace_form(client, p, key("x")) else { panic!() };
            p = next;
            assert!(p.error.is_none());
            for _ in 0..2 {
                let Mode::Prompt(next) = server.on_workspace_form(client, p, key("tab")) else { panic!() };
                p = next;
            }
            assert_eq!(p.workspace_form.as_ref().unwrap().field, 2);
            assert!(matches!(server.on_workspace_form(client, p, key("enter")), Mode::Normal));
            let p = server.add_workspace_form(client, None);
            let layout = form_layout(&p, server.client_screen(client));
            let click = |area: Rect| {
                InputEvent::Mouse(MouseEvent {
                    kind: MouseKind::Press(MouseButton::Left),
                    col: area.x,
                    row: area.y,
                    mods: Default::default(),
                })
            };
            assert!(matches!(server.on_workspace_form(client, p, click(layout.cancel)), Mode::Normal));
            let p = Prompt::new("Add workspace", "test", PromptAction::NewWorkspace);
            let layout = form_layout(&p, server.client_screen(client));
            let Mode::Prompt(p) = server.on_workspace_form(client, p, click(layout.primary)) else { panic!() };
            assert_eq!(p.error.as_deref(), Some("This name is already used."));
        }
    }
}
