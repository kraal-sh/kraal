//! Workspace names use the same fields, buttons and return path as other management forms.

use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::ui::{Mode, Picker, PickerState, Prompt, PromptAction};
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
}

fn verb(p: &Prompt) -> &'static str {
    if p.error.is_some() {
        "Retry"
    } else if matches!(p.action, PromptAction::NewWorkspace) {
        "Add"
    } else {
        "Rename"
    }
}

fn buttons(p: &Prompt) -> forms::Buttons<'static> {
    forms::Buttons {
        primary: forms::Button { field: 1, label: verb(p) },
        alternate: None,
        cancel: forms::Button { field: 2, label: "Cancel" },
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
    let message = p.error.clone().unwrap_or_else(|| {
        p.workspace_form
            .as_ref()
            .and_then(|f| f.cwd.as_deref())
            .map(|cwd| format!("Starts in {}", crate::paths::shorten_home(cwd)))
            .unwrap_or_default()
    });
    forms::FormLayout::new(screen, 1, &message, &buttons(p))
}

pub(super) fn draw_form(p: &Prompt, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let form = p.workspace_form.as_ref()?;
    let layout = form_layout(p, screen);
    overlays::frame(layout.area, &p.title, crate::theme::current().accent, buf);
    let cursor = forms::draw_text_field(layout.fields[0], "Name    ", &p.value, p.cursor, "", form.field == 0, buf);
    forms::draw_message(&layout, p.scroll, buf);
    let action = if form.field == 2 { "Cancel" } else { verb(p) };
    forms::draw_buttons(&layout, &buttons(p), form.field, form.pending, action, buf);
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
                p.restore(state);
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
        form.cwd = self
            .client_focus(id)
            .and_then(|p| self.panes.get(&p))
            .and_then(|p| p.current_cwd())
            .or_else(|| self.current_ws(id).and_then(|(_, wi)| self.workspaces[wi].cwd.clone()))
            .or_else(|| std::env::current_dir().ok());
        form.back = from.map(Picker::state);
        p
    }

    pub(super) fn rename_workspace_form(&self, ws: u32, from: Option<&Picker>) -> Mode {
        let Some(wi) = self.ws_index(ws) else {
            return self.workspace_picker(0).result("Workspace unavailable");
        };
        let mut p = Prompt::new("Rename workspace", &self.workspaces[wi].name, PromptAction::RenameWorkspace(ws));
        p.workspace_form.as_mut().unwrap().back = from.map(Picker::state);
        Mode::Prompt(p)
    }

    fn workspace_name_used(&self, name: &str, except: Option<u32>) -> bool {
        self.workspaces.iter().any(|w| Some(w.id) != except && w.name == name)
            || self.unrestored.iter().any(|w| w.name == name)
            || self.definitions().iter().any(|(path, def)| {
                !matches!(self.definition_workspace(path), Ok(Some(_)))
                    && def.as_ref().map_or_else(|_| path.file_stem().is_some_and(|s| s == name), |def| def.name == name)
            })
    }

    pub(super) fn on_workspace_form(&mut self, id: ClientId, mut p: Prompt, event: InputEvent) -> Mode {
        let layout = form_layout(&p, self.client_screen(id));
        let buttons = buttons(&p);
        p.scroll = p.scroll.min(layout.max_scroll);
        let before = p.value.clone();
        let error = p.error.clone();
        let form = p.workspace_form.as_mut().unwrap();
        let mut fields = [forms::Field {
            id: 0,
            area: layout.fields[0],
            value: forms::FieldValue::Text { value: &mut p.value, cursor: &mut p.cursor },
        }];
        let outcome = forms::FormState {
            field: &mut form.field,
            scroll: &mut p.scroll,
            error: &mut p.error,
            pending: &mut form.pending,
        }
        .handle(event, &mut fields, &buttons, &layout);
        if p.value == before && p.error.is_none() {
            p.error = error;
        } else if p.value != before {
            p.scroll = 0;
        }
        match outcome {
            forms::FormEvent::Cancel => self.workspace_form_back(id, p, None),
            forms::FormEvent::Submit => self.submit_workspace_form(id, p),
            _ => Mode::Prompt(p),
        }
    }

    fn workspace_form_back(&self, id: ClientId, mut p: Prompt, renamed: Option<u32>) -> Mode {
        let Some(state) = p.workspace_form.as_mut().and_then(|f| f.back.take()) else { return Mode::Normal };
        let mut picker = self.workspace_picker(self.clients.get(&id).map_or(0, |c| c.workspace));
        picker.restore(state);
        if let Some(ws) = renamed {
            select_workspace(&mut picker, ws);
            picker.result("Workspace renamed")
        } else {
            Mode::Picker(picker)
        }
    }

    pub(super) fn submit_workspace_form(&mut self, id: ClientId, mut p: Prompt) -> Mode {
        let name = p.value.trim();
        let target = match p.action {
            PromptAction::RenameWorkspace(ws) => Some(ws),
            PromptAction::NewWorkspace => None,
            _ => unreachable!("a workspace name form"),
        };
        let result = if name.is_empty() {
            Err("Type a name.".into())
        } else if self.workspace_name_used(name, target) {
            Err("This name is already used.".into())
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
