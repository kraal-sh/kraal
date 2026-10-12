//! Paste buffers in the session: every copy is kept (see [`crate::buffers`]), listed in a picker
//! to paste again, and reached from the command line (`kraal buffer`).

use super::persistence::Persist;
use super::ui::{PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Server};
use crate::buffers;
use crate::layout::PaneId;
use crate::pane::{PaneSpec, ShownText};
use crate::protocol::{BufferInfo, Placement, Response};
use crate::text::human_size;

impl Server {
    /// Keep what was just copied as the newest buffer.
    pub(super) fn keep_copy(&mut self, text: &str) {
        if self.buffers.copy(text, crate::accounts::now()) {
            self.buffers_dirty = true;
        }
    }

    /// The buffers to choose from, newest first; Enter pastes one into the focused pane.
    pub(super) fn buffer_picker(&self) -> Picker {
        let now = crate::accounts::now();
        let sep = if crate::theme::current().ascii { " | " } else { " ⏎ " };
        let items = (0..self.buffers.list().len())
            .map(|i| {
                let b = &self.buffers.list()[i];
                let mut detail = format!("{} · {}", self.buffers.key(i), human_size(b.text.len() as u64));
                let lines = buffers::lines(&b.text);
                if lines > 1 {
                    detail.push_str(&format!(" · {lines} lines"));
                }
                detail.push_str(&format!(" · {}", buffers::age(now - b.at)));
                PickItem::new(buffers::one_line(&b.text, sep, 56), detail, PickAction::Buffer(b.id))
            })
            .collect();
        Picker::new(PickerKind::Buffers, "Paste buffers · newest first", items)
    }

    /// The text of the buffer a picker's line points at; when it went meanwhile, the client is
    /// told.
    pub(super) fn picked_buffer(&mut self, client: ClientId, buffer: u64) -> Option<String> {
        let text = self.buffers.by_id(buffer).map(|b| b.text.clone());
        if text.is_none() {
            self.toast(client, "that buffer is gone");
        }
        text
    }

    /// Show a buffer in a floating preview pane of the client's workspace (never saved with the
    /// session).
    pub(super) fn show_buffer(&mut self, client: ClientId, buffer: u64) {
        let Some(text) = self.picked_buffer(client, buffer) else { return };
        let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return };
        let title = format!("buffer {}", self.buffers.key_of(buffer).unwrap_or_default());
        let spec = PaneSpec { text: Some(ShownText { title: title.clone(), text }), ..Default::default() };
        if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(title)) {
            self.toast(client, format!("{e:#}"));
        }
    }

    pub(super) fn delete_picked_buffer(&mut self, client: ClientId, buffer: u64) {
        let key = self.buffers.key_of(buffer);
        match self.buffers.delete_id(buffer) {
            Some(_) => {
                self.buffers_dirty = true;
                self.toast(client, format!("deleted buffer {}", key.unwrap_or_default()));
            }
            None => self.toast(client, "that buffer is gone"),
        }
    }

    /// The buffers as `kraal buffer list` shows them, newest first.
    pub(super) fn buffer_infos(&self) -> Vec<BufferInfo> {
        (0..self.buffers.list().len())
            .map(|i| {
                let b = &self.buffers.list()[i];
                BufferInfo {
                    buffer: self.buffers.key(i),
                    named: b.name.is_some(),
                    bytes: b.text.len(),
                    lines: buffers::lines(&b.text),
                    at: b.at,
                    preview: buffers::one_line(&b.text, " | ", 72),
                }
            })
            .collect()
    }

    /// `kraal buffer set`.
    pub(super) fn set_buffer(&mut self, name: Option<&str>, text: String) -> Result<Response, String> {
        if let Some(name) = name {
            buffers::check_name(name)?;
        }
        if text.is_empty() {
            return Err("nothing to keep: the text is empty".into());
        }
        self.buffers.set(name, text, crate::accounts::now())?;
        self.buffers_dirty = true;
        Ok(Response::Ok)
    }

    /// `kraal buffer delete`.
    pub(super) fn delete_buffer(&mut self, key: Option<&str>, all: bool) -> Result<Response, String> {
        if all {
            self.buffers.clear();
        } else {
            self.buffers.delete(key)?;
        }
        self.buffers_dirty = true;
        Ok(Response::Ok)
    }

    /// `kraal buffer paste`: the buffer goes to the pane as a paste, bracketed when its program
    /// asked for that. Whether the pane may get it (production) is the caller's to check.
    pub(super) fn paste_buffer_to(&mut self, pane: PaneId, key: Option<&str>) -> Result<Response, String> {
        let text = self.buffers.get(key)?.text.clone();
        let label = self.pane_label(pane);
        let pn = self.panes.get_mut(&pane).ok_or("no such pane")?;
        if !pn.is_terminal() {
            return Err(format!("{label} runs no program to paste into"));
        }
        if pn.exit_code.is_some() {
            return Err("the pane's program has exited".into());
        }
        let bytes = crate::input::encode_paste(text.as_bytes(), pn.input_modes());
        pn.write_input(bytes);
        Ok(Response::Ok)
    }

    /// Take back the buffers kept by the last server, when they are kept; otherwise make sure no
    /// file holds them any more.
    pub(super) fn load_buffers(&mut self) {
        let path = crate::paths::buffers_path(&self.session);
        if !self.config.buffers.persist {
            if path.exists() {
                let _ = self.persister.send(Persist::Buffers(path, None));
            }
            return;
        }
        let data = match crate::paths::read_regular(&path, (self.config.buffers.max_bytes() as u64).saturating_mul(8)) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.warnings.push(format!("paste buffers: {}: {e}", crate::paths::shorten_home(&path)));
                return;
            }
        };
        if let Err(e) = self.buffers.load_json(&data) {
            self.warnings.push(format!("paste buffers: {}: {e}", crate::paths::shorten_home(&path)));
        }
    }

    /// Write the buffers that changed to their file, when they are kept across restarts.
    pub(super) fn save_buffers(&mut self) {
        if !std::mem::take(&mut self.buffers_dirty) || !self.config.buffers.persist {
            return;
        }
        let path = crate::paths::buffers_path(&self.session);
        let _ = self.persister.send(Persist::Buffers(path, Some(self.buffers.to_json())));
    }

    /// The `[buffers]` settings changed: new limits apply at once, and the file is written or
    /// deleted as `persist` now says.
    pub(super) fn reconfigure_buffers(&mut self, persisted: bool) {
        let cfg = self.config.buffers.clone();
        self.buffers.set_limits(cfg.limit, cfg.max_bytes());
        match (persisted, cfg.persist) {
            (true, false) => {
                let _ = self.persister.send(Persist::Buffers(crate::paths::buffers_path(&self.session), None));
            }
            // Written again: the new limits may have dropped some.
            (_, true) => self.buffers_dirty = true,
            _ => {}
        }
        self.save_buffers();
    }
}

/// Write the buffers' file, readable by its owner only, or delete it (`None`).
pub(super) fn write_file(path: &std::path::Path, data: Option<&[u8]>) -> std::io::Result<()> {
    match data {
        Some(data) => {
            if let Some(dir) = path.parent() {
                crate::paths::ensure_private_dir(dir)?;
            }
            crate::paths::write_private(path, data)
        }
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}
