//! Workspace names use the same fields, buttons and return path as other management forms.

use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind, PickerState, Prompt};
use super::workspace_lifecycle::DeleteTarget;
use super::{ClientId, Server, forms, overlays};
use crate::ansi::Cursor;
use crate::input::InputEvent;
use crate::theme::NoticeKind;

type WorkspacePrompt = Prompt<Box<WorkspaceForm>>;

pub enum WorkspaceAction {
    New,
    Rename(u32),
    Duplicate(u32),
    Restore(String),
    Review(u32),
    Import,
    Export(u32),
}

pub struct WorkspaceForm {
    pub action: WorkspaceAction,
    field: usize,
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
    source_message: String,
    source_details: Option<String>,
    details: bool,
    pub(super) undo: bool,
    undo_layout: Option<Box<crate::persist::WorkspaceSnap>>,
}

impl WorkspaceForm {
    pub fn new(action: WorkspaceAction) -> Box<Self> {
        Box::new(Self {
            action,
            field: 0,
            cwd: None,
            back: None,
            invalid_name: false,
            file: String::new(),
            file_cursor: 0,
            preview: None,
            choices: Vec::new(),
            account: 0,
            existing: None,
            result: None,
            source_message: String::new(),
            source_details: None,
            details: false,
            undo: false,
            undo_layout: None,
        })
    }
}

fn name_form_error(error: crate::workspace::NameError) -> String {
    match error {
        crate::workspace::NameError::Used => "This name is already used.".into(),
        _ => error.to_string(),
    }
}

fn verb(p: &WorkspacePrompt) -> &'static str {
    if matches!(p.data.action, WorkspaceAction::Review(_)) {
        let form = p.data.as_ref();
        return if form.undo {
            "Undo load"
        } else if form.source_details.is_some() {
            "Open saved"
        } else {
            "Load from file"
        };
    }
    if p.data.result.is_some() {
        return "Close";
    }
    if matches!(p.data.action, WorkspaceAction::Export(_)) {
        if p.error.is_some() { "Retry" } else { "Export" }
    } else if matches!(p.data.action, WorkspaceAction::Import) {
        let form = p.data.as_ref();
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
    } else if matches!(p.data.action, WorkspaceAction::Restore(_)) {
        "Restore"
    } else if p.error.is_some() && !p.data.invalid_name {
        "Retry"
    } else if matches!(p.data.action, WorkspaceAction::New) {
        "Add"
    } else if matches!(p.data.action, WorkspaceAction::Duplicate(_)) {
        "Duplicate"
    } else {
        "Rename"
    }
}

fn field_count(p: &WorkspacePrompt) -> usize {
    if matches!(p.data.action, WorkspaceAction::Review(_)) {
        return 0;
    }
    if p.data.result.is_some() {
        return 0;
    }
    if matches!(p.data.action, WorkspaceAction::Import) {
        2 + p.data.preview.as_ref().map_or(0, |p| usize::from(!p.accounts.is_empty()))
    } else {
        1
    }
}

fn buttons(p: &WorkspacePrompt) -> forms::Buttons<'static> {
    let count = field_count(p);
    if p.data.result.is_some() {
        return forms::Buttons {
            primary: forms::Button { field: 0, label: "Close" },
            alternate: None,
            cancel: forms::Button { field: 0, label: "Close" },
        };
    }
    if matches!(p.data.action, WorkspaceAction::Review(_)) {
        let form = p.data.as_ref();
        return forms::Buttons {
            primary: forms::Button { field: 0, label: verb(p) },
            alternate: (!form.undo).then_some(forms::Button {
                field: 2,
                label: if form.source_details.is_some() {
                    if form.details { "Retry" } else { "Details" }
                } else {
                    "Keep layout"
                },
            }),
            cancel: forms::Button { field: 1, label: if form.source_details.is_some() { "Close" } else { "Cancel" } },
        };
    }
    forms::Buttons {
        primary: forms::Button { field: count, label: verb(p) },
        alternate: matches!(p.data.action, WorkspaceAction::New)
            .then_some(forms::Button { field: 3, label: "From file..." }),
        cancel: forms::Button { field: count + 1, label: if count == 0 { "Close" } else { "Cancel" } },
    }
}

fn select_workspace(picker: &mut Picker, ws: u32) {
    picker.reveal(|i| i.action.workspace() == Some(ws));
}

pub(super) fn form_layout(p: &WorkspacePrompt, screen: Rect) -> forms::FormLayout {
    let mut message =
        p.data.cwd.as_deref().map(|cwd| format!("Starts in {}", crate::paths::shorten_home(cwd))).unwrap_or_default();
    if matches!(p.data.action, WorkspaceAction::Duplicate(_)) {
        message = "Copies the layout.\nPrograms wait for you to start them.".into();
    } else if matches!(p.data.action, WorkspaceAction::Export(_)) {
        message = "Workspaces are saved automatically.\nExports the layout, without conversations.\nAgents use portable references, without private account names.".into();
    } else if matches!(p.data.action, WorkspaceAction::Import) {
        message = p.data.preview.as_ref().map(|p| p.summary()).unwrap_or_default();
    }
    if let Some(account) = p.data.preview.as_ref().and_then(|preview| preview.accounts.get(p.data.account)) {
        message.push_str(&format!("\nChoose a {} account for {}.", account.agent, account.reference));
    }
    if matches!(p.data.action, WorkspaceAction::Review(_)) {
        let form = p.data.as_ref();
        message = form.source_message.clone();
        if form.details
            && let Some(details) = &form.source_details
        {
            message.push_str(&format!("\n{details}"));
        }
    }
    if let Some(result) = p.data.result.as_ref() {
        message = result.clone();
    }
    let error = p.error.as_deref().filter(|_| p.data.result.is_none());
    let mut layout = forms::FormLayout::new(screen, field_count(p), &message, error, &buttons(p));
    if matches!(p.data.action, WorkspaceAction::Export(_) | WorkspaceAction::Review(_) | WorkspaceAction::Import) {
        // File paths in errors and results must retain spaces that prose wrapping would collapse.
        layout.lines = message.lines().flat_map(|l| crate::text::fold(l, layout.body.width as usize)).collect();
        layout.error_start = layout.lines.len();
        layout.lines.extend(error.unwrap_or("").lines().flat_map(|l| crate::text::fold(l, layout.body.width as usize)));
        layout.max_scroll = layout.lines.len().saturating_sub(layout.body.height as usize);
    }
    if p.data.source_details.is_some() {
        // An unreadable source is a failed result, before Retry sets the form error.
        layout.error_start = 0;
    }
    layout
}

pub(super) fn draw_form(p: &WorkspacePrompt, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let form = p.data.as_ref();
    let layout = form_layout(p, screen);
    overlays::frame(layout.area, &p.title, crate::theme::current().accent, buf);
    let mut cursor = None;
    if form.result.is_none() && !matches!(p.data.action, WorkspaceAction::Review(_)) {
        let importing = matches!(p.data.action, WorkspaceAction::Import);
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
        let label = if matches!(p.data.action, WorkspaceAction::Export(_)) { "File    " } else { "Name    " };
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
        buttons.alternate.as_ref().unwrap().label
    } else {
        verb(p)
    };
    forms::draw_buttons(&layout, &buttons, form.field, None, action, buf);
    cursor
}

impl Server {
    pub(super) fn delete_workspace(&mut self, client: ClientId, target: DeleteTarget) -> Mode {
        let result = match &target {
            DeleteTarget::Workspace(ws) => self.keep_and_delete_workspace(*ws),
            DeleteTarget::Source(path) => self.delete_workspace_source(path),
        };
        match result {
            Ok(identity) => {
                self.refresh_workspace_pickers();
                self.mark_dirty();
                self.toast(client, NoticeKind::Notice, "Workspace deleted · Undo");
                if let Some(c) = self.clients.get_mut(&client) {
                    c.toast_action = Some((0, super::attention::ToastAction::RestoreWorkspace(identity.clone())));
                }
                let mut picker = self.workspace_picker(self.clients.get(&client).map_or(0, |c| c.workspace));
                picker.undo = Some(identity);
                picker.result(NoticeKind::Notice, "Workspace deleted")
            }
            Err(message) => {
                if let DeleteTarget::Workspace(ws) = target
                    && let Some(wi) = self.ws_index(ws)
                    && self.workspaces[wi].delete_pending
                {
                    self.workspaces[wi].close_error = Some(message.clone());
                }
                self.refresh_workspace_pickers();
                self.mark_dirty();
                Mode::confirm(message, ConfirmAction::RetryDeleteWorkspace(target))
            }
        }
    }

    pub(super) fn deleted_workspace_picker(&self) -> Picker {
        let items = self
            .deleted_workspaces
            .iter()
            .rev()
            .map(|d| {
                let (tabs, panes) = d.workspace.counts();
                let days = crate::accounts::now().saturating_sub(d.deleted_at).max(0) / 86400;
                let when = if days == 0 {
                    "Deleted today".into()
                } else {
                    format!("Deleted {days} day{} ago", super::input::plural(days as usize))
                };
                PickItem::new(
                    &d.workspace.name,
                    format!(
                        "{when} · {tabs} tab{} · {panes} pane{}",
                        super::input::plural(tabs),
                        super::input::plural(panes)
                    ),
                    PickAction::RestoreWorkspace(d.workspace.id.clone()),
                )
            })
            .collect();
        Picker::new(PickerKind::DeletedWorkspaces, "Deleted workspaces", items)
    }

    pub(super) fn restore_workspace_mode(&mut self, client: ClientId, identity: &str, from: Option<&Picker>) -> Mode {
        let Some(saved) = self.deleted_workspaces.iter().find(|d| d.workspace.id == identity) else {
            return self.deleted_workspace_picker().result(NoticeKind::Notice, "Workspace already restored");
        };
        let name = saved.workspace.name.clone();
        if self.workspace_name_used(&name, None, None, &self.definitions()) {
            let base = format!("{name}-restored");
            let proposed = self.available_workspace_name(&base);
            let mut p = Prompt::new(
                "Restore workspace",
                &proposed,
                WorkspaceForm::new(WorkspaceAction::Restore(identity.into())),
            );
            p.error = Some(format!("{name} is already used. Choose another name."));
            self.workspace_restore_back(&mut p, from);
            return Mode::WorkspaceForm(p);
        }
        self.restore_workspace_result(client, identity, &name, from)
    }

    pub(super) fn restore_workspace_result(
        &mut self,
        client: ClientId,
        identity: &str,
        name: &str,
        from: Option<&Picker>,
    ) -> Mode {
        let had_source =
            self.deleted_workspaces.iter().any(|d| d.workspace.id == identity && d.workspace.definition.is_some());
        match self.restore_deleted_workspace(identity, name) {
            Ok(ws) => {
                self.refresh_workspace_pickers();
                self.mark_dirty();
                self.toast(client, NoticeKind::Notice, "Workspace restored");
                let mut picker = self.workspace_picker(0);
                picker.select(|i| i.action.workspace() == Some(ws));
                let workspace = &self.workspaces[self.ws_index(ws).unwrap()];
                if had_source && workspace.definition.is_none() {
                    picker.result(
                        NoticeKind::Warning,
                        "Workspace restored · Source belongs to another workspace · Enter Open",
                    )
                } else {
                    picker.result(NoticeKind::Notice, "Workspace restored · Enter Open")
                }
            }
            Err(error) => {
                let mut p = Prompt::new(
                    "Restore workspace",
                    name,
                    WorkspaceForm::new(WorkspaceAction::Restore(identity.into())),
                );
                p.error = Some(error);
                self.workspace_restore_back(&mut p, from);
                Mode::WorkspaceForm(p)
            }
        }
    }

    pub(super) fn ask_delete_workspace(&self, target: DeleteTarget) -> Mode {
        let Some(warning) = self.delete_workspace_warning(&target) else {
            return self.workspace_picker(0).result(NoticeKind::Warning, "Workspace unavailable");
        };
        let action = if warning.retry {
            ConfirmAction::RetryDeleteWorkspace(target)
        } else {
            ConfirmAction::DeleteWorkspace { target, stop: warning.stop }
        };
        Mode::confirm(warning.message, action)
    }

    pub(super) fn return_to_workspaces(&self, id: ClientId, mode: Mode, state: PickerState) -> Mode {
        match mode {
            Mode::Confirm(mut q) => {
                q.workspace_back = Some(Box::new(state));
                Mode::Confirm(q)
            }
            Mode::Picker(mut p) => {
                let workspace = p.current().and_then(|i| i.action.workspace());
                let message = p.message.clone();
                let message_kind = p.message_kind;
                let undo = p.undo.clone();
                p.restore(state);
                if message.is_some() {
                    p.message = message;
                    p.message_kind = message_kind;
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

    pub(super) fn add_workspace_form(&self, id: ClientId, from: Option<&Picker>) -> WorkspacePrompt {
        let name = from.map(|p| p.query.trim()).filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(|| {
            let definitions = self.definitions();
            (1..)
                .map(|n| format!("workspace-{n}"))
                .find(|n| !self.workspace_name_used(n, None, None, &definitions))
                .unwrap()
        });
        let mut p = Prompt::new("Add workspace", &name, WorkspaceForm::new(WorkspaceAction::New));
        let form = p.data.as_mut();
        form.cwd = self.workspace_form_cwd(id);
        form.back = from.and_then(Picker::form_back);
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
            return self.workspace_picker(0).result(NoticeKind::Warning, "Workspace unavailable");
        };
        let mut p =
            Prompt::new("Rename workspace", &self.workspaces[wi].name, WorkspaceForm::new(WorkspaceAction::Rename(ws)));
        p.data.back = from.and_then(Picker::form_back);
        Mode::WorkspaceForm(p)
    }

    pub(super) fn duplicate_workspace_form(&self, ws: u32, from: Option<&Picker>) -> Mode {
        let Some(wi) = self.ws_index(ws) else {
            return self
                .workspace_picker(0)
                .result(NoticeKind::Warning, "Workspace unavailable. Select a workspace to duplicate.");
        };
        let base = format!("{}-copy", self.workspaces[wi].name);
        let name = self.available_workspace_name(&base);
        let mut p = Prompt::new("Duplicate workspace", &name, WorkspaceForm::new(WorkspaceAction::Duplicate(ws)));
        p.data.back = from.and_then(Picker::form_back);
        Mode::WorkspaceForm(p)
    }

    pub(super) fn workspace_restore_back(&self, p: &mut WorkspacePrompt, from: Option<&Picker>) {
        p.data.back = from.and_then(Picker::form_back);
    }

    pub(super) fn on_workspace_form(&mut self, id: ClientId, mut p: WorkspacePrompt, event: InputEvent) -> Mode {
        let layout = form_layout(&p, self.client_screen(id));
        let buttons = buttons(&p);
        p.scroll = p.scroll.min(layout.max_scroll);
        let importing = matches!(p.data.action, WorkspaceAction::Import);
        let before = p.value.clone();
        let error = p.error.clone();
        let form = p.data.as_mut();
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
        if form.result.is_none() && !matches!(form.action, WorkspaceAction::Review(_)) {
            fields.push(forms::Field {
                id: usize::from(importing),
                area: layout.fields[usize::from(importing)],
                value: forms::FieldValue::Text { value: &mut p.value, cursor: &mut p.cursor },
            });
        }
        if importing && form.preview.as_ref().is_some_and(|p| !p.accounts.is_empty()) {
            fields.push(forms::Field { id: 2, area: layout.fields[2], value: forms::FieldValue::Choice });
        }
        let outcome =
            forms::FormState { field: &mut form.field, scroll: &mut p.scroll, error: &mut p.error, pending: &mut None }
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
            forms::FormEvent::Submit if matches!(form.action, WorkspaceAction::New) && form.field == 3 => {
                form.action = WorkspaceAction::Import;
                p.value.clear();
                p.cursor = 0;
                p.error = None;
                form.field = 0;
                Mode::WorkspaceForm(p)
            }
            forms::FormEvent::Submit if matches!(form.action, WorkspaceAction::Review(_)) => {
                self.submit_source_form(id, p)
            }
            forms::FormEvent::Submit => self.submit_workspace_form(id, p),
            forms::FormEvent::Choice { .. } if importing => {
                let i = form.account;
                let choices = &form.preview.as_ref().unwrap().accounts[i].choices;
                form.choices[i] = (form.choices[i] + 1) % (choices.len() + 1);
                p.error = None;
                Mode::WorkspaceForm(p)
            }
            _ if importing && old_field == 0 && form.field == 1 && form.preview.is_none() => {
                self.preview_import_form(p)
            }
            _ => Mode::WorkspaceForm(p),
        }
    }

    pub(super) fn import_source_form(&self, id: ClientId, path: &std::path::Path, from: &Picker) -> Mode {
        let mut p = self.add_workspace_form(id, Some(from));
        p.data.action = WorkspaceAction::Import;
        p.value = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        p.cursor = p.value.chars().count();
        p.data.file = path.display().to_string();
        p.data.file_cursor = p.data.file.chars().count();
        self.preview_import_form(p)
    }

    pub(super) fn undo_source_layout(&mut self, client: ClientId, ws: u32, from: Option<&Picker>) -> Mode {
        match self.review_source_form(ws, from, true) {
            Mode::WorkspaceForm(p) => self.load_source_form(client, p, None),
            mode => mode,
        }
    }

    pub(super) fn review_source_form(&self, ws: u32, from: Option<&Picker>, undo: bool) -> Mode {
        let Some(wi) = self.ws_index(ws) else {
            return self.workspace_picker(0).result(NoticeKind::Warning, "Workspace unavailable");
        };
        let workspace = &self.workspaces[wi];
        let mut p = Prompt::new(
            &format!("{} · {}", workspace.source_status.unwrap_or("Load from file"), workspace.name),
            "",
            WorkspaceForm::new(WorkspaceAction::Review(ws)),
        );
        let form = p.data.as_mut();
        form.field = 1;
        form.back = from.and_then(Picker::form_back);
        form.undo = undo;
        if undo {
            form.undo_layout = workspace.layout_undo.clone();
            p.title = format!("Undo load · {}", workspace.name);
            let (tabs, panes) = workspace.layout_undo.as_ref().map_or((0, 0), |w| w.counts());
            form.source_message = format!(
                "Restore the layout from before the last Load from file?\nPrevious layout: {tabs} tabs · {panes} panes\nThe restored workspace will be Closed. Programs will not restart."
            );
            if workspace.layout_undo.is_none() {
                return self.workspace_picker(ws).result(NoticeKind::Notice, "This replacement was already undone.");
            }
            return Mode::WorkspaceForm(p);
        }
        let Some(path) = &workspace.definition else {
            return self.workspace_picker(ws).result(NoticeKind::Notice, "This workspace has no source file.");
        };
        let (tabs, panes) = workspace.counts();
        match self.preview_source(path) {
            Ok(preview) => {
                form.source_message = format!(
                    "{}\nSaved workspace: {tabs} tab{} · {panes} pane{}\nFile: {}\nThe current layout will be kept for Undo.",
                    crate::paths::shorten_home(path),
                    if tabs == 1 { "" } else { "s" },
                    if panes == 1 { "" } else { "s" },
                    preview.summary()
                );
                form.preview = Some(Box::new(preview));
            }
            Err(e) => {
                p.title = format!("File error · {}", workspace.name);
                form.source_message = format!(
                    "Couldn't read {}.\n{}\nYour saved workspace is unchanged.\nFix the file, then Retry in Details.",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    if super::workspace_sources::source_missing(&e) {
                        "Source file missing.".into()
                    } else {
                        e.root_cause().to_string()
                    }
                );
                form.source_details = Some(format!("{}\n{e:#}", path.display()));
            }
        }
        Mode::WorkspaceForm(p)
    }

    fn refresh_source_form(&self, mut p: WorkspacePrompt, message: Option<String>) -> Mode {
        let WorkspaceAction::Review(ws) = p.data.action else { unreachable!() };
        let old = p.data.as_mut();
        let Mode::WorkspaceForm(mut next) = self.review_source_form(ws, None, old.undo) else {
            return self.workspace_picker(ws).result(NoticeKind::Warning, "Workspace unavailable");
        };
        next.data.back = old.back.take();
        next.error = message;
        Mode::WorkspaceForm(next)
    }

    fn submit_source_form(&mut self, id: ClientId, mut p: WorkspacePrompt) -> Mode {
        let WorkspaceAction::Review(ws) = p.data.action else { unreachable!() };
        let form = p.data.as_mut();
        if form.source_details.is_some() {
            if form.field == 2 {
                if form.details {
                    return self.refresh_source_form(p, None);
                }
                form.details = true;
                p.scroll = 0;
                return Mode::WorkspaceForm(p);
            }
            return match self.reopen_workspace(ws) {
                Ok(()) => {
                    self.show_workspace(id, ws);
                    Mode::Normal
                }
                Err(e) => {
                    p.error = Some(format!("{e:#}"));
                    Mode::WorkspaceForm(p)
                }
            };
        }
        if form.field == 2 {
            return match self.keep_source_layout(ws, form.preview.as_ref().unwrap()) {
                Ok(()) => self.workspace_form_back(id, p, None),
                Err(e) => self.refresh_source_form(p, Some(format!("{e:#}"))),
            };
        }
        self.load_source_form(id, p, None)
    }

    pub(super) fn load_source_form(
        &mut self,
        id: ClientId,
        mut p: WorkspacePrompt,
        approved: Option<Vec<super::persistence::ClosePane>>,
    ) -> Mode {
        let WorkspaceAction::Review(ws) = p.data.action else { unreachable!() };
        let Some(wi) = self.ws_index(ws) else {
            return self.workspace_picker(0).result(NoticeKind::Warning, "Workspace unavailable");
        };
        if self.workspaces[wi].delete_pending
            || self.workspaces[wi].close_error.is_some()
            || self.workspaces[wi].kept.as_ref().is_some_and(|s| s.close_pending.is_some())
        {
            return self
                .workspace_picker(ws)
                .result(NoticeKind::Warning, "Finish Close or Delete, then retry the layout change.");
        }
        let form = p.data.as_ref();
        let undo = form.undo;
        if undo
            && serde_json::to_value(&form.undo_layout).ok()
                != serde_json::to_value(&self.workspaces[wi].layout_undo).ok()
        {
            return self.refresh_source_form(p, Some("The retained layout changed. Review Undo again.".into()));
        }
        let was_open = !self.workspaces[wi].closed();
        let viewers: Vec<_> = self.clients.values().filter(|c| c.workspace == ws).map(|c| c.id).collect();
        if let Some(preview) = &form.preview
            && (self.workspaces[wi].definition.as_ref() != Some(&preview.path)
                || crate::paths::FileRevision::read(&preview.path).ok().as_ref() != Some(&preview.revision))
        {
            return self
                .refresh_source_form(p, Some("File changed again. Review the current file before loading.".into()));
        }
        if form.preview.as_ref().is_some_and(|p| !p.accounts.is_empty()) {
            return self.refresh_source_form(p, Some("The file refers to an unavailable account. Fix the account in the source file or add it in Accounts, then Load from file again.".into()));
        }
        let inventory = self.source_close_inventory(wi);
        if let Some(stops) = self.stops(Some(ws))
            && approved.as_ref() != Some(&inventory)
        {
            return Mode::confirm(
                format!(
                    "{}\n{stops}\nThe current layout will be kept{}.\nNew commands will wait for you to start them.",
                    if undo { "Undo load?" } else { "Load layout?" },
                    if undo { " until the replacement is saved" } else { " for Undo" }
                ),
                super::ui::ConfirmAction::LoadWorkspace { prompt: p, inventory },
            );
        }
        match self.replace_source_layout(ws, form.preview.as_deref(), Some(&inventory)) {
            Ok(()) => {
                if was_open && !undo {
                    if let Err(e) = self.reopen_workspace(ws) {
                        return self.workspace_picker(ws).result(
                            NoticeKind::Error,
                            format!(
                                "Layout loaded. Couldn't open it: {e:#}. Retry Open or choose Undo load in Actions."
                            ),
                        );
                    }
                    for viewer in viewers {
                        self.show_workspace(viewer, ws);
                    }
                }
                self.toast(
                    id,
                    NoticeKind::Notice,
                    if undo { "Layout restored · Open from Workspaces" } else { "Layout loaded · Undo" },
                );
                if !undo && let Some(c) = self.clients.get_mut(&id) {
                    c.toast_action = Some((0, super::attention::ToastAction::UndoLayout(ws)));
                }
                let mode = self.source_result(id, ws, undo);
                match p.data.back.take() {
                    Some(back) => self.return_to_workspaces(id, mode, back),
                    None => mode,
                }
            }
            Err(e) => {
                if self.workspaces[wi].close_error.is_some() {
                    return self.workspace_picker(ws).result(NoticeKind::Error, format!("{e:#}"));
                }
                self.refresh_source_form(p, Some(format!("{e:#}")))
            }
        }
    }

    pub(super) fn export_workspace_form(&self, id: ClientId, ws: u32, from: Option<&Picker>) -> Mode {
        let Some(wi) = self.ws_index(ws) else {
            return self.workspace_picker(0).result(NoticeKind::Warning, "Workspace unavailable");
        };
        let file = crate::paths::shorten_home(&self.export_file_default(wi));
        let mut p = Prompt::new("Export layout", &file, WorkspaceForm::new(WorkspaceAction::Export(ws)));
        let form = p.data.as_mut();
        form.cwd = self.workspace_form_cwd(id);
        form.back = from.and_then(Picker::form_back);
        Mode::WorkspaceForm(p)
    }

    fn workspace_form_path(p: &WorkspacePrompt, value: &str) -> PathBuf {
        let form = p.data.as_ref();
        let path = crate::paths::expand_tilde(value.trim());
        if path.is_relative() { form.cwd.as_ref().map_or(path.clone(), |cwd| cwd.join(&path)) } else { path }
    }

    fn preview_import_form(&self, mut p: WorkspacePrompt) -> Mode {
        let path = Self::workspace_form_path(&p, &p.data.file);
        let form = p.data.as_mut();
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
        Mode::WorkspaceForm(p)
    }

    pub(super) fn exported_layout(&self, mut p: WorkspacePrompt, path: &std::path::Path) -> Mode {
        p.title = "Layout exported".into();
        p.error = None;
        p.scroll = 0;
        let form = p.data.as_mut();
        form.result = Some(crate::paths::shorten_home(path));
        form.field = 0;
        Mode::WorkspaceForm(p)
    }

    pub(super) fn workspace_form_back(&self, id: ClientId, mut p: WorkspacePrompt, renamed: Option<u32>) -> Mode {
        let back = p.data.back.take();
        let Mode::Picker(mut picker) = self.form_back(id, back.clone()) else { return Mode::Normal };
        if let Some(ws) = renamed {
            select_workspace(&mut picker, ws);
            picker.close_to = back.map(Box::new);
            picker.result(NoticeKind::Notice, "Workspace renamed")
        } else {
            Mode::Picker(picker)
        }
    }

    pub(super) fn submit_workspace_form(&mut self, id: ClientId, mut p: WorkspacePrompt) -> Mode {
        if p.data.result.is_some() {
            return self.workspace_form_back(id, p, None);
        }
        if let WorkspaceAction::Export(ws) = p.data.action {
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
                            Mode::WorkspaceForm(p)
                        }
                    }
                }
                Err(e) => {
                    p.error = Some(format!("{e:#}"));
                    Mode::WorkspaceForm(p)
                }
            };
        }
        if matches!(p.data.action, WorkspaceAction::Import) {
            let form = p.data.as_ref();
            if let Some(ws) = form.existing {
                return match self.reopen_workspace(ws) {
                    Ok(()) => {
                        self.show_workspace(id, ws);
                        Mode::Normal
                    }
                    Err(e) => {
                        p.error = Some(format!("{e:#}"));
                        Mode::WorkspaceForm(p)
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
                    p.data.preview = None;
                }
                return Mode::WorkspaceForm(p);
            }
            if form.account + 1 < preview.accounts.len() {
                let form = p.data.as_mut();
                form.account += 1;
                form.field = 2;
                return Mode::WorkspaceForm(p);
            }
            let choices = preview
                .accounts
                .iter()
                .enumerate()
                .filter_map(|(i, a)| {
                    Some((a.reference.clone(), a.choices.get(form.choices[i].checked_sub(1)?)?.0.clone()))
                })
                .collect::<Vec<_>>();
            let result = crate::workspace::validate_name(&p.value, false)
                .map_err(anyhow::Error::new)
                .and_then(|()| self.import_layout(&preview.path, p.value.trim(), &preview.revision, &choices));
            return match result {
                Ok(ws) => {
                    self.show_workspace(id, ws);
                    self.toast(id, NoticeKind::Notice, "Workspace added. Commands wait for you to start them.");
                    Mode::Normal
                }
                Err(e) => {
                    p.error = Some(format!("{e:#}"));
                    if crate::paths::FileRevision::read(&preview.path).ok().as_ref() != Some(&preview.revision) {
                        p.data.preview = None;
                    }
                    Mode::WorkspaceForm(p)
                }
            };
        }
        if let Err(error) = crate::workspace::validate_name(&p.value, false) {
            p.data.invalid_name = true;
            p.error = Some(error.to_string());
            p.scroll = 0;
            return Mode::WorkspaceForm(p);
        }
        let name = p.value.trim();
        if let WorkspaceAction::Duplicate(source) = p.data.action {
            let validation = self.validate_workspace_name(name, None);
            p.data.invalid_name = validation.is_err();
            let result = validation.map_err(name_form_error).and_then(|()| self.duplicate_workspace(source, name));
            return match result {
                Ok(ws) => {
                    self.refresh_workspace_pickers();
                    self.mark_dirty();
                    self.toast(id, NoticeKind::Notice, "Workspace duplicated");
                    let mut picker = self.workspace_picker(self.clients.get(&id).map_or(0, |c| c.workspace));
                    let back = p.data.back.take();
                    if let Some(state) = back.clone() {
                        picker.restore(state);
                    }
                    picker.close_to = back.map(Box::new);
                    select_workspace(&mut picker, ws);
                    picker.result(NoticeKind::Notice, "Workspace duplicated · Enter Open")
                }
                Err(error) => {
                    p.error = Some(error);
                    p.scroll = 0;
                    Mode::WorkspaceForm(p)
                }
            };
        }
        if let WorkspaceAction::Restore(identity) = &p.data.action {
            let result = self.restore_deleted_workspace(identity, name);
            return match result {
                Ok(ws) => {
                    self.refresh_workspace_pickers();
                    self.mark_dirty();
                    self.toast(id, NoticeKind::Notice, "Workspace restored");
                    let mut picker = self.workspace_picker(0);
                    picker.select(|i| i.action.workspace() == Some(ws));
                    picker.result(NoticeKind::Notice, "Workspace restored · Enter Open")
                }
                Err(error) => {
                    p.error = Some(error);
                    p.scroll = 0;
                    Mode::WorkspaceForm(p)
                }
            };
        }
        let target = match p.data.action {
            WorkspaceAction::Rename(ws) => Some(ws),
            WorkspaceAction::New => None,
            _ => unreachable!("a workspace name form"),
        };
        let validation = self.validate_workspace_name(name, target);
        p.data.invalid_name = validation.is_err();
        let result = if let Err(error) = validation {
            Err(name_form_error(error))
        } else if let Some(ws) = target {
            self.rename_workspace(ws, name)
        } else {
            let cwd = p.data.cwd.clone();
            self.add_workspace(name, cwd).map(|ws| self.show_workspace(id, ws))
        };
        match result {
            Ok(()) => {
                self.refresh_workspace_pickers();
                self.mark_dirty();
                self.toast(
                    id,
                    NoticeKind::Notice,
                    if target.is_some() { "Workspace renamed" } else { "Workspace added" },
                );
                if target.is_some() { self.workspace_form_back(id, p, target) } else { Mode::Normal }
            }
            Err(error) => {
                p.error = Some(error);
                p.scroll = 0;
                Mode::WorkspaceForm(p)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, KeySpec, MouseButton, MouseEvent, MouseKind};
    use crate::pane::PaneSpec;
    use crate::protocol::Placement;
    use crate::server::ui::PickAction;

    fn key(spec: &str) -> InputEvent {
        let k = KeySpec::parse(spec).unwrap();
        InputEvent::Key(Key { code: k.code, mods: k.mods, raw: Vec::new() })
    }

    fn confirm(server: &mut Server, client: ClientId, q: super::super::ui::Question, bytes: &[u8]) -> Mode {
        server.clients.get_mut(&client).unwrap().mode = Mode::Confirm(q);
        server.client_input(client, bytes);
        std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal)
    }

    fn click(area: Rect) -> InputEvent {
        InputEvent::Mouse(MouseEvent {
            kind: MouseKind::Press(MouseButton::Left),
            col: area.x,
            row: area.y,
            mods: Default::default(),
        })
    }

    #[test]
    fn workspace_forms_and_export_close_return_to_their_origin() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        for size in [(80, 24), (124, 36)] {
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            for origin in 0..3 {
                let mut list = server.workspace_picker(ws);
                list.query = "tes".into();
                list.cursor = 1;
                list.refilter();
                list.standalone_actions = origin == 1;
                let from = (origin != 2).then_some(&list);
                for operation in 0..6 {
                    for leave in 0..3 {
                        let mode = match operation {
                            0 | 4 => Mode::WorkspaceForm(server.add_workspace_form(client, from)),
                            1 => server.rename_workspace_form(ws, from),
                            2 => server.duplicate_workspace_form(ws, from),
                            _ => server.export_workspace_form(client, ws, from),
                        };
                        let Mode::WorkspaceForm(mut p) = mode else { panic!("expected a form") };
                        if operation == 4 {
                            let layout = form_layout(&p, server.client_screen(client));
                            let Mode::WorkspaceForm(next) =
                                server.on_workspace_form(client, p, click(layout.alternate.unwrap()))
                            else {
                                panic!()
                            };
                            p = next;
                            assert!(matches!(p.data.action, WorkspaceAction::Import));
                        } else if operation == 5 {
                            let Mode::WorkspaceForm(next) =
                                server.exported_layout(p, std::path::Path::new("/tmp/layout.kdl"))
                            else {
                                panic!()
                            };
                            p = next;
                        }
                        let layout = form_layout(&p, server.client_screen(client));
                        let event = match leave {
                            0 => key("esc"),
                            1 => click(layout.cancel),
                            _ => {
                                p.data.field = buttons(&p).cancel.field;
                                key("enter")
                            }
                        };
                        forms::assert_return(server.on_workspace_form(client, p, event), from);
                    }
                }
            }
        }
    }

    #[test]
    fn permission_errors_stay_file_errors_in_the_list_and_initial_review() {
        use std::os::unix::fs::PermissionsExt;
        let (mut server, path, ws) = super::super::workspace_sources::tests::setup("permission");
        let parent = path.parent().unwrap();
        let permissions = std::fs::metadata(parent).unwrap().permissions();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = server.preview_source(&path).unwrap_err();
        server.refresh_source_status();
        let status = server.workspaces[0].source_status;
        let mode = server.review_source_form(ws, None, false);
        std::fs::set_permissions(parent, permissions).unwrap();
        assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(status, Some("File error"));
        let Mode::WorkspaceForm(prompt) = mode else { panic!() };
        let form = prompt.data;
        assert!(form.source_message.contains(&error.root_cause().to_string()));
        assert!(!form.source_message.contains("Source file missing"));
        assert!(!form.details);
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn source_review_buttons_diagnosis_retry_and_cancel_work_at_both_sizes() {
        for size in [(80, 24), (124, 36)] {
            let (mut server, path, ws) = super::super::workspace_sources::tests::setup(&format!("review-{}", size.0));
            let (client, _peer) = super::super::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            c.workspace = ws;
            std::fs::write(&path, "layout { pane command=\"new\"; pane; }").unwrap();
            server.scan_definitions();
            if size.0 == 124 {
                let c = server.clients.get_mut(&client).unwrap();
                c.sidebar = Some(true);
                c.toast = None;
                let mut buf = Buffer::empty(server.client_screen(client));
                server.compose(client, 0, &mut buf);
                assert!(crate::testing::buffer_text(&buf).contains("File changed"));
            }
            let mut list = server.workspace_picker(ws);
            list.query = "personal".into();
            list.refilter();
            let Mode::WorkspaceForm(p) = server.review_source_form(ws, Some(&list), false) else { panic!() };
            let mut buf = Buffer::empty(server.client_screen(client));
            draw_form(&p, buf.area, &mut buf);
            for expected in [
                "File changed",
                "Saved workspace: 1 tab · 1 pane",
                "File: 1 tab · 2 panes",
                "Keep layout",
                "Load from file",
            ] {
                assert!(crate::testing::buffer_text(&buf).contains(expected), "{}", crate::testing::buffer_text(&buf));
            }
            let Mode::Picker(back) = server.on_workspace_form(client, p, key("enter")) else {
                panic!("Cancel is selected first")
            };
            assert_eq!(back.query, "personal");
            assert_eq!(server.workspaces[0].source_status, Some("File changed"));
            let Mode::WorkspaceForm(p) = server.review_source_form(ws, Some(&list), false) else { panic!() };
            let area = form_layout(&p, server.client_screen(client)).alternate.unwrap();
            let click = |area: Rect| {
                InputEvent::Mouse(MouseEvent {
                    kind: MouseKind::Press(MouseButton::Left),
                    col: area.x,
                    row: area.y,
                    mods: Default::default(),
                })
            };
            assert!(matches!(server.on_workspace_form(client, p, click(area)), Mode::Picker(_)));
            assert_eq!(server.workspaces[0].source_status, None);
            std::fs::write(&path, "layout {\n pane {\n").unwrap();
            server.scan_definitions();
            let Mode::WorkspaceForm(p) = server.review_source_form(ws, Some(&list), false) else { panic!() };
            let layout = form_layout(&p, server.client_screen(client));
            let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, click(layout.alternate.unwrap())) else {
                panic!()
            };
            assert!(p.data.details);
            let layout = form_layout(&p, server.client_screen(client));
            let mut buf = Buffer::empty(server.client_screen(client));
            draw_form(&p, buf.area, &mut buf);
            assert_eq!(buf[layout.body.as_position()].fg, crate::theme::current().blocked);
            let lines = layout.lines.join("\n");
            assert!(lines.contains("line"), "{lines}");
            assert!(lines.contains("Your saved workspace is unchanged"));
            assert!(lines.contains("source.kdl"));
            assert_eq!(buttons(&p).alternate.unwrap().label, "Retry");
            std::fs::write(&path, "layout { pane command=\"fixed\"; }").unwrap();
            let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert!(p.data.source_details.is_none());
            assert!(p.data.preview.is_some());
            assert_eq!(server.panes.len(), 1);
            std::fs::remove_file(&path).unwrap();
            let Mode::WorkspaceForm(p) = server.review_source_form(ws, Some(&list), false) else { panic!() };
            let layout = form_layout(&p, server.client_screen(client));
            assert!(matches!(server.on_workspace_form(client, p, click(layout.primary)), Mode::Normal));
            assert_eq!(server.panes.len(), 1);
            assert!(!path.exists());
            let workspaces = server.workspaces.len();
            let invalid = path.with_file_name("new-source.kdl");
            std::fs::write(&invalid, "layout {").unwrap();
            let list = server.workspace_picker(ws);
            let item = list
                .items
                .iter()
                .find(|i| matches!(&i.action, PickAction::UnavailableDefinition(p) if p == &invalid))
                .unwrap()
                .clone();
            let Mode::WorkspaceForm(mut p) = server.pick(client, &list, item) else { panic!() };
            assert!(matches!(p.data.action, WorkspaceAction::Import));
            assert!(p.error.as_ref().unwrap().contains("No workspace was added"));
            p.value = "chosen name".into();
            std::fs::write(&invalid, "layout { pane command=\"true\"; }").unwrap();
            let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!(p.value, "chosen name");
            assert!(p.data.preview.is_some());
            assert_eq!(server.workspaces.len(), workspaces);
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn load_confirmation_rechecks_the_file_and_programs_and_cancel_stops_nothing() {
        let _batch = crate::pane::batch();
        let (mut server, path, ws) = super::super::workspace_sources::tests::setup("confirm");
        let (client, _peer) = super::super::tests::client(&mut server);
        server.clients.get_mut(&client).unwrap().workspace = ws;
        server
            .spawn_pane_in(
                ws,
                None,
                PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() },
                Placement::Tab,
                None,
            )
            .unwrap();
        std::fs::write(&path, "layout { pane command=\"new\"; }").unwrap();
        let Mode::WorkspaceForm(p) = server.review_source_form(ws, None, false) else { panic!() };
        let Mode::Confirm(q) = server.load_source_form(client, p, None) else { panic!() };
        assert!(q.message.contains("sleep"));
        assert!(q.message.contains("kept for Undo"));
        assert!(!q.yes);
        let Mode::WorkspaceForm(p) = confirm(&mut server, client, q, b"n") else { panic!() };
        assert_eq!(server.panes.len(), 2);
        let Mode::Confirm(q) = server.load_source_form(client, p, None) else { panic!() };
        std::fs::write(&path, "layout { pane command=\"newer\"; pane; }").unwrap();
        let Mode::WorkspaceForm(p) = confirm(&mut server, client, q, b"y") else { panic!() };
        assert!(p.error.as_ref().unwrap().contains("File changed again"));
        assert_eq!(p.data.preview.as_ref().unwrap().definition.panes().len(), 2);
        assert_eq!(server.panes.len(), 2);
        let Mode::Confirm(q) = server.load_source_form(client, p, None) else { panic!() };
        server
            .spawn_pane_in(
                ws,
                None,
                PaneSpec { command: vec!["sleep".into(), "90".into()], ..Default::default() },
                Placement::Tab,
                None,
            )
            .unwrap();
        let Mode::Confirm(q) = confirm(&mut server, client, q, b"y") else { panic!() };
        assert!(q.message.contains("sleep 90"));
        assert!(!q.yes);
        assert_eq!(server.panes.len(), 3);
        assert!(!server.workspaces[0].closed());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn loading_an_open_workspace_keeps_its_viewers_and_waits_to_run_commands() {
        let (mut server, path, ws) = super::super::workspace_sources::tests::setup("open-load");
        let (client, _peer) = super::super::tests::client(&mut server);
        let (other, _other_peer) = super::super::tests::client(&mut server);
        for id in [client, other] {
            server.clients.get_mut(&id).unwrap().workspace = ws;
        }
        server.new_workspace("another workspace".into(), None);
        std::fs::write(&path, "layout { pane command=\"never-run\"; }").unwrap();
        let Mode::WorkspaceForm(p) = server.review_source_form(ws, None, false) else { panic!() };
        let mode = server.load_source_form(client, p, None);
        assert!(matches!(mode, Mode::Picker(_)), "Load from file returned {}", mode.label());
        assert!(!server.workspaces[0].closed());
        assert!(server.panes.values().all(|p| p.suspended));
        for id in [client, other] {
            assert_eq!(server.clients[&id].workspace, ws);
        }
        let mode = server.toast_action(client, 0, super::super::attention::ToastAction::UndoLayout(ws));
        assert!(
            matches!(mode, Mode::Picker(_)),
            "UndoLayout at width {} returned {}",
            server.clients[&client].cols,
            mode.label()
        );
        assert!(server.workspaces[0].closed());
        assert_eq!(server.workspaces[0].counts(), (1, 1));
        assert!(server.workspaces[0].layout_undo.is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn undo_notification_confirms_a_shell_whose_activity_cannot_yet_be_checked() {
        // Hold the launch to reproduce Undo arriving before the shell's process is available.
        let _batch = crate::pane::batch();
        let (mut server, path, ws) = super::super::workspace_sources::tests::setup("undo-starting-shell");
        let (client, _peer) = super::super::tests::client(&mut server);
        server.clients.get_mut(&client).unwrap().workspace = ws;
        std::fs::write(&path, "layout { pane command=\"never-run\"; pane; }").unwrap();
        let Mode::WorkspaceForm(p) = server.review_source_form(ws, None, false) else { panic!() };
        assert!(matches!(server.load_source_form(client, p, None), Mode::Picker(_)));
        let panes = server.workspaces[0].panes();
        assert!(panes.iter().any(|id| !server.panes[id].suspended && server.panes[id].pid().is_none()));
        let mode = server.toast_action(client, 0, super::super::attention::ToastAction::UndoLayout(ws));
        let Mode::Confirm(q) = mode else {
            panic!("UndoLayout at width {}: expected Confirm, got {}", server.clients[&client].cols, mode.label());
        };
        assert!(q.message.contains("Undo load?"));
        assert!(q.message.contains("Unknown program (activity could not be checked)"));
        assert!(!q.yes);
        assert!(matches!(confirm(&mut server, client, q, b"n"), Mode::Normal));
        assert_eq!(server.workspaces[0].panes(), panes);
        assert!(!server.workspaces[0].closed());
        assert!(server.workspaces[0].layout_undo.is_some());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_close_returns_to_the_workspace_with_retry_close() {
        let (mut server, path, ws) = super::super::workspace_sources::tests::setup("close-result");
        std::fs::write(&path, "layout { pane command=\"new\"; }").unwrap();
        super::super::persistence::tests::writer(&mut server, vec![Ok(()), Err("disk full".into())]);
        let Mode::WorkspaceForm(p) = server.review_source_form(ws, None, false) else { panic!() };
        let Mode::Picker(p) = server.load_source_form(0, p, None) else { panic!() };
        assert!(matches!(p.current().unwrap().action, PickAction::RetryWorkspaceClose(id) if id == ws));
        assert_eq!(p.action_keys(), [("enter", "Retry Close")]);
        assert!(server.workspaces[0].closed());
        assert_eq!(server.workspaces[0].counts(), (1, 1));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_closed_load_has_durable_undo_and_a_stale_undo_reviews_the_latest_layout() {
        let (mut server, path, ws) = super::super::workspace_sources::tests::setup("ui-undo");
        server.keep_and_close_workspace(ws).unwrap();
        std::fs::write(&path, "layout { pane command=\"first\"; }").unwrap();
        let Mode::WorkspaceForm(p) = server.review_source_form(ws, None, false) else { panic!() };
        let Mode::Picker(p) = server.load_source_form(0, p, None) else { panic!() };
        assert!(p.action_keys().contains(&("", "Undo load")));
        let Mode::WorkspaceForm(undo) = server.review_source_form(ws, Some(&p), true) else { panic!() };
        std::fs::write(&path, "layout { pane command=\"second\"; pane command=\"third\"; }").unwrap();
        let preview = server.preview_source(&path).unwrap();
        server.replace_source_layout(ws, Some(&preview), None).unwrap();
        let Mode::WorkspaceForm(undo) = server.load_source_form(0, undo, None) else { panic!() };
        assert!(undo.error.as_ref().unwrap().contains("retained layout changed"));
        assert!(matches!(server.load_source_form(0, undo, None), Mode::Picker(_)));
        assert!(server.workspaces[0].closed());
        assert_eq!(server.workspaces[0].counts(), (1, 1));
        server.reopen_workspace(ws).unwrap();
        assert!(server.panes.values().all(|p| p.suspended));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
            let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, event) else { panic!() };
            p = next;
            assert!(matches!(p.data.action, WorkspaceAction::Import));
            p.data.file = path.display().to_string();
            p.value = "personal".into();
            let Mode::WorkspaceForm(mut p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert!(p.error.as_ref().unwrap().contains("No workspace was added"));
            assert_eq!(p.value, "personal");
            std::fs::write(&path, "layout { pane command=\"true\"; }").unwrap();
            let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            p = next;
            assert!(p.data.preview.is_some());
            assert!(server.panes.is_empty());
            let mut buf = Buffer::empty(server.client_screen(client));
            draw_form(&p, buf.area, &mut buf);
            for expected in ["File", "Name    personal", "1 tab · 1 pane", "Commands wait", "Enter Add"] {
                assert!(crate::testing::buffer_text(&buf).contains(expected), "{}", crate::testing::buffer_text(&buf));
            }
            p.value = "personal\n".into();
            let Mode::WorkspaceForm(mut p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!(p.value, "personal\n");
            assert_eq!(p.error.as_deref(), Some("Choose a name without control characters."));
            assert!(p.data.preview.is_some());
            assert!(server.panes.is_empty());
            p.value = "personal".into();
            super::super::persistence::tests::writer(&mut server, vec![Ok(())]);
            assert!(matches!(server.on_workspace_form(client, p, key("enter")), Mode::Normal));
            assert!(server.panes.values().all(|p| p.suspended));
            let mut p = Prompt::new("Add workspace", "", WorkspaceForm::new(WorkspaceAction::Import));
            p.data.file = path.display().to_string();
            let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
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
        let mut p = Prompt::new("Add workspace", "chosen-name", WorkspaceForm::new(WorkspaceAction::Import));
        p.data.file = path.display().to_string();
        let Mode::WorkspaceForm(next) = server.preview_import_form(p) else { panic!() };
        p = next;
        for n in 0..30 {
            for (cols, rows) in [(80, 24), (124, 36)] {
                let area = Rect::new(0, 0, cols, rows);
                let layout = form_layout(&p, area);
                assert!(layout.area.bottom() < rows);
                assert_eq!(layout.fields.len(), 3);
                let mut buf = Buffer::empty(area);
                draw_form(&p, area, &mut buf);
                assert!(crate::testing::buffer_text(&buf).contains(&format!("claude:missing-{n}")));
            }
            assert_eq!(p.data.choices[n], 0);
            p.data.field = 2;
            let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, key("right")) else { panic!() };
            p = next;
            assert_eq!(p.data.choices[n], 1);
            if n < 29 {
                assert_eq!(verb(&p), "Next");
                let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
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
        let Mode::WorkspaceForm(mut p) = server.export_workspace_form(client, ws, Some(&list)) else { panic!() };
        p.value = path.display().to_string();
        p.cursor = 4;
        let Mode::Confirm(q) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        assert!(!q.yes);
        assert!(q.message.contains("Comments and unsupported KDL"));
        let Mode::WorkspaceForm(p) = confirm(&mut server, client, q, b"\r") else { panic!() };
        assert_eq!(p.cursor, 4);
        let Mode::Confirm(q) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        std::fs::write(&path, "second edit").unwrap();
        let Mode::WorkspaceForm(p) = confirm(&mut server, client, q, b"y") else { panic!() };
        assert!(p.error.as_ref().unwrap().contains("changed while saving"));
        for size in [(80, 24), (124, 36)] {
            let mut buf = Buffer::empty(Rect::new(0, 0, size.0, size.1));
            draw_form(&p, buf.area, &mut buf);
            assert!(crate::testing::buffer_text(&buf).contains("Enter Retry"));
            assert!(crate::testing::buffer_text(&buf).contains("Workspaces are saved automatically."));
            let layout = form_layout(&p, buf.area);
            assert!(layout.lines.join("").contains(p.error.as_ref().unwrap()));
            assert_eq!(buf[layout.body.as_position()].fg, crate::theme::current().text);
            assert_eq!(
                buf[(layout.body.x, layout.body.y + layout.error_start as u16)].fg,
                crate::theme::current().blocked
            );
        }
        let Mode::Confirm(q) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        std::fs::create_dir(&lock).unwrap();
        let Mode::WorkspaceForm(p) = confirm(&mut server, client, q, b"y") else { panic!() };
        assert!(p.error.is_some());
        assert_eq!(p.value, path.display().to_string());
        std::fs::remove_dir(&lock).unwrap();
        let Mode::Confirm(q) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        let Mode::WorkspaceForm(p) = confirm(&mut server, client, q, b"y") else { panic!() };
        assert_eq!(p.title, "Layout exported");
        assert_eq!(p.data.result.as_deref(), Some(path.canonicalize().unwrap().to_str().unwrap()));
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
            let Mode::WorkspaceForm(mut prompt) = server.export_workspace_form(client, ws, None) else { panic!() };
            assert_eq!(prompt.data.cwd.as_ref(), Some(&dir));
            server.panes.get_mut(&pane).unwrap().reported_cwd = Some(dir.join("elsewhere"));
            prompt.value = value.into();
            let Mode::WorkspaceForm(result) = server.submit_workspace_form(client, prompt) else { panic!() };
            assert_eq!(result.title, "Layout exported", "{:?}", result.error);
            let path = dir.join("copy.kdl");
            assert!(path.is_file());
            assert_eq!(crate::workspace::load_file(&path).unwrap().panes()[0].command.as_deref(), Some("true"));
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn add_workspace_buttons_share_one_rendered_row() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            let p = Prompt::new("Add workspace", "web", WorkspaceForm::new(WorkspaceAction::New));
            let area = Rect::new(0, 0, cols, rows);
            let mut buf = Buffer::empty(area);
            draw_form(&p, area, &mut buf);
            let shown = crate::testing::buffer_text(&buf);
            assert_eq!(buf[form_layout(&p, area).primary.as_position()].bg, crate::theme::current().accent);
            assert!(
                buf.content.chunks(cols as usize).any(|row| {
                    let row: String = row.iter().map(|c| c.symbol()).collect();
                    row.contains(" Add     From file...     Cancel ")
                }),
                "{shown}"
            );
        }
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
            let Mode::WorkspaceForm(p) = server.export_workspace_form(client, ws, Some(&list)) else { panic!() };
            let Mode::WorkspaceForm(mut p) = server.exported_layout(p, &path) else { panic!() };
            let area = Rect::new(0, 0, cols, rows);
            let layout = form_layout(&p, area);
            assert!(layout.fields.is_empty());
            assert!(layout.max_scroll > 0);
            assert_eq!(layout.lines.join(""), path.to_str().unwrap());
            p.scroll = layout.max_scroll;
            let mut buf = Buffer::empty(area);
            assert!(draw_form(&p, area, &mut buf).is_none());
            for expected in ["Layout exported", "shared.kdl", "Enter Close", "Esc Close"] {
                assert!(crate::testing::buffer_text(&buf).contains(expected), "{}", crate::testing::buffer_text(&buf));
            }
            assert_eq!(layout.primary, layout.cancel);
            assert_eq!(buf[layout.cancel.as_position()].bg, crate::theme::current().accent);
            assert_eq!(buf[layout.body.as_position()].fg, crate::theme::current().text);
            assert!(
                !crate::testing::buffer_text(&buf).contains("Done")
                    && !crate::testing::buffer_text(&buf).contains("Tab Next")
            );
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
        let mut kept = crate::server::persistence::snapshot(&server).workspaces.remove(1);
        kept.closed = true;
        server.workspaces[1].kept = Some(kept);
        server.new_workspace("test-copy-2".into(), None);
        let failed = crate::server::persistence::snapshot(&server).workspaces.remove(2);
        server.workspaces.pop();
        server.unrestored.push(failed);
        let dir = std::env::temp_dir().join(format!("kraal-duplicate-name-{}", crate::identity::new_uuid()));
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
            let Mode::WorkspaceForm(p) = server.duplicate_workspace_form(ws, Some(&list)) else { panic!() };
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
                assert!(crate::testing::buffer_text(&buf).contains(expected));
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
            let Mode::WorkspaceForm(mut p) = server.duplicate_workspace_form(ws, Some(&list)) else { panic!() };
            for value in ["", "test", "copy\n"] {
                p.value = value.into();
                let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
                assert!(next.error.is_some());
                assert_eq!(verb(&next), "Duplicate");
                p = next;
            }
            p.value = "independent".into();
            p.cursor = 3;
            let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!((p.value.as_str(), p.cursor), ("independent", 3));
            assert_eq!(verb(&p), "Retry");
            assert!(p.error.as_ref().unwrap().contains("No workspace was added"));
            let mut buf = Buffer::empty(server.client_screen(client));
            draw_form(&p, buf.area, &mut buf);
            assert!(crate::testing::buffer_text(&buf).contains("disk full"));
            assert!(crate::testing::buffer_text(&buf).contains("Programs wait for you to start them."));
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
            server.clients.get_mut(&client).unwrap().mode = Mode::Picker(p);
            server.client_input(client, b"\x1b");
            server.flush_client_input(client);
            let mode = std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal);
            forms::assert_return(mode, Some(&list));
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
            for name in ["taken", " ", "new\n"] {
                for rename in [false, true] {
                    let mut p = if rename {
                        let Mode::WorkspaceForm(p) = server.rename_workspace_form(ws, None) else { panic!() };
                        p
                    } else {
                        server.add_workspace_form(client, None)
                    };
                    p.value = name.into();
                    let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
                    let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
                    draw_form(&p, buf.area, &mut buf);
                    let shown: Vec<String> =
                        buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
                    let verb = if rename { "Rename" } else { "Add" };
                    assert!(shown.iter().any(|row| row.contains(p.error.as_ref().unwrap())));
                    assert_eq!(buf[form_layout(&p, buf.area).primary.as_position()].bg, crate::theme::current().accent);
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
        let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
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
            let shown = crate::testing::buffer_text(&buf);
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
    fn add_reads_the_definitions_once_to_find_a_free_name() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let (client, _peer) = super::super::tests::client(&mut server);
        let dir = crate::testing::TempDir::new("add-free-name");
        for n in 1..=15 {
            dir.write(&format!("workspace-{n}.kdl"), "layout { pane; }");
        }
        server.config.workspace_dirs.push(dir.text());
        crate::workspace::SOURCE_SCANS.set(0);
        assert_eq!(server.add_workspace_form(client, None).value, "workspace-16");
        assert_eq!(crate::workspace::SOURCE_SCANS.get(), server.config.workspace_dirs().len());
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
                let mut snap = crate::server::persistence::snapshot(&server).workspaces.remove(0);
                snap.closed = true;
                server.workspaces[0].kept = Some(snap);
            }
            let saved = super::super::persistence::tests::writer(&mut server, vec![Err("disk full".into()), Ok(())]);
            let mut list = server.workspace_picker(ws);
            list.query = "test".into();
            list.cursor = 2;
            list.refilter();
            let Mode::WorkspaceForm(mut p) = server.rename_workspace_form(ws, Some(&list)) else { panic!() };
            p.value = "test/review".into();
            p.cursor = 5;
            let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
            assert_eq!((p.value.as_str(), p.cursor), ("test/review", 5));
            assert!(p.error.as_ref().unwrap().contains("Still named test."));
            for (cols, rows) in [(80, 24), (124, 36)] {
                let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
                draw_form(&p, buf.area, &mut buf);
                let shown: Vec<String> =
                    buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
                assert_eq!(buf[form_layout(&p, buf.area).primary.as_position()].bg, crate::theme::current().accent);
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
                let mut snap = crate::server::persistence::snapshot(&server).workspaces.remove(0);
                snap.closed = true;
                server.workspaces[0].kept = Some(snap);
            }
            server.new_workspace("other".into(), None);
            let saved = super::super::persistence::tests::writer(&mut server, vec![Ok(())]);
            let mut list = server.workspace_picker(ws);
            list.query = "zebra".into();
            list.cursor = 2;
            list.refilter();
            let Mode::WorkspaceForm(mut p) = server.rename_workspace_form(ws, Some(&list)) else { panic!() };
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
        let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        assert_eq!((p.value.as_str(), p.cursor), ("new workspace", 3));
        let error = p.error.clone().unwrap();
        assert!(error.contains(&format!("The starting folder {} is no longer available", dir.display())));
        assert!(error.contains("Restore it and retry, or cancel"));
        assert_eq!(p.data.cwd.as_ref(), Some(&dir));
        assert_eq!((server.workspaces.len(), server.panes.len()), (1, 1));
        assert_eq!(server.clients[&client].workspace, ws);
        assert!(server.clients[&client].toast.is_none());
        assert!(saved.lock().unwrap().is_empty());
        let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("left")) else { panic!() };
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
            let before = crate::server::persistence::snapshot(&server);
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
                let Mode::WorkspaceForm(p) = server.rename_workspace_form(ws, None) else { panic!() };
                p
            } else {
                server.add_workspace_form(client, None)
            };
            p.value = "rejected".into();
            p.cursor = 3;
            let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
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
        let mut snap = crate::server::persistence::snapshot(&server).workspaces.remove(0);
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
                        let Mode::WorkspaceForm(p) = server.rename_workspace_form(ws, None) else { panic!() };
                        p
                    }
                };
                p.value = name.into();
                let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
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
        let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key("enter")) else { panic!() };
        assert!(p.error.as_ref().unwrap().contains("Couldn't save the workspace. No workspace was added."));
        for (cols, rows) in [(80, 24), (124, 36)] {
            let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
            draw_form(&p, buf.area, &mut buf);
            let shown: Vec<String> =
                buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
            assert_eq!(buf[form_layout(&p, buf.area).primary.as_position()].bg, crate::theme::current().accent);
            assert!(shown.iter().any(|row| row.contains("Enter Retry")));
            assert!(shown.iter().any(|row| row.contains("Starts in")));
            let layout = form_layout(&p, buf.area);
            assert_eq!(buf[layout.body.as_position()].fg, crate::theme::current().text);
            assert_eq!(
                buf[(layout.body.x, layout.body.y + layout.error_start as u16)].fg,
                crate::theme::current().blocked
            );
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
            let Mode::WorkspaceForm(mut p) = server.rename_workspace_form(ws, None) else { panic!() };
            p.value = "café 日".into();
            p.cursor = 4;
            p.error = Some(format!("{}Final detail", "Long diagnostic with 日本語. ".repeat(200)));
            let layout = form_layout(&p, server.client_screen(client));
            assert!(layout.max_scroll > 0);
            for _ in 0..100 {
                let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, key("pagedown")) else { panic!() };
                p = next;
            }
            assert_eq!(p.scroll, layout.max_scroll);
            let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, key("left")) else { panic!() };
            p = next;
            assert!(p.error.is_some());
            let mut buf = Buffer::empty(server.client_screen(client));
            let cursor = draw_form(&p, buf.area, &mut buf).unwrap();
            assert!(layout.fields[0].contains((cursor.x, cursor.y).into()));
            let shown = crate::testing::buffer_text(&buf);
            for expected in ["café", "Final detail", "Enter Retry", "PgUp/PgDn Scroll", "Esc Cancel"] {
                assert!(shown.contains(expected), "{shown}");
            }
            let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, key("x")) else { panic!() };
            p = next;
            assert!(p.error.is_none());
            for _ in 0..2 {
                let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, key("tab")) else { panic!() };
                p = next;
            }
            assert_eq!(p.data.field, 2);
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
            let p = Prompt::new("Add workspace", "test", WorkspaceForm::new(WorkspaceAction::New));
            let layout = form_layout(&p, server.client_screen(client));
            let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, click(layout.primary)) else { panic!() };
            assert_eq!(p.error.as_deref(), Some("This name is already used."));
        }
    }
}
