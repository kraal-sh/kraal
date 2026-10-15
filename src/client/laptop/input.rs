use std::sync::Arc;

use ratatui::layout::Rect;

use super::Controller;
use crate::client::paste;
use crate::input::{InputEvent, KeyCode};
use crate::protocol::ClientMsg;

impl Controller {
    pub fn pending_keys(&self) -> bool {
        self.state.lock().unwrap().parser.has_pending()
    }

    pub fn input(self: &Arc<Self>, bytes: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap();
        // Observe every input chunk, including pastes that started before consent opened.
        let events = if bytes.is_empty() { state.parser.flush() } else { state.parser.feed(bytes) };
        if state.prompt.is_none() {
            return false;
        }
        let mut answer = None;
        for event in events {
            let Some(prompt) = state.prompt.as_mut() else {
                break;
            };
            let buttons = if prompt.replacement.is_some() { 2 } else { 3 };
            match event {
                InputEvent::Key(key) => match key.code {
                    KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
                        if prompt.replacement.is_some() =>
                    {
                        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                        let view = self.view(prompt);
                        let (max_scroll, page) = view.scroll_limits(Rect::new(0, 0, cols, rows.saturating_sub(1)));
                        let step = if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) { page } else { 1 };
                        prompt.scroll = if matches!(key.code, KeyCode::Up | KeyCode::PageUp) {
                            prompt.scroll.min(max_scroll).saturating_sub(step)
                        } else {
                            prompt.scroll.saturating_add(step).min(max_scroll)
                        };
                    }
                    KeyCode::Tab | KeyCode::Right => prompt.selected = (prompt.selected + 1) % buttons,
                    KeyCode::BackTab | KeyCode::Left => prompt.selected = (prompt.selected + buttons - 1) % buttons,
                    KeyCode::Enter => {
                        answer = Some(prompt.selected);
                        break;
                    }
                    KeyCode::Esc => {
                        answer = Some(buttons - 1);
                        break;
                    }
                    KeyCode::Char('c') if key.mods.ctrl => {
                        answer = Some(buttons - 1);
                        break;
                    }
                    _ => {}
                },
                InputEvent::Mouse(mouse)
                    if mouse.kind == crate::input::MouseKind::Press(crate::input::MouseButton::Left) =>
                {
                    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                    let (_, buttons) = self.view(prompt).render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
                    answer = buttons.iter().position(|r| r.contains((mouse.col, mouse.row).into()));
                    if answer.is_some() {
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(answer) = answer {
            let prompt = state.prompt.take().unwrap();
            if let Some((_, reply)) = prompt.replacement {
                let _ = reply.try_send(answer == 0);
                drop(state);
                self.send(&ClientMsg::Repaint);
                return true;
            }
            let retry = if prompt.request.copy.is_some() { "retry the copy" } else { "press r" };
            let mut error = None;
            if answer == 0 {
                state.allowed = true;
            }
            if answer == 1
                && let Some((ssh, label)) = &self.destination
            {
                error = paste::remember_laptop(&self.permissions, ssh, label).err().map(|e| {
                    format!("Cannot save laptop permission: {e}. Fix laptop-files.toml on the laptop and {retry}")
                });
            }
            state.busy = answer != 2 && error.is_none();
            let generation = state.generation;
            drop(state);
            self.send(&ClientMsg::Repaint);
            if answer == 2 {
                let host = self.identity.as_ref().map_or("laptop", |i| i.host.as_str());
                let message = if prompt.request.copy.is_some() {
                    format!("{host} refused: nothing was copied. Press c to ask again")
                } else {
                    format!("{host} refused: nothing was listed. Press r to ask again")
                };
                self.reply(prompt.request, Err(message));
                self.next();
            } else if let Some(error) = error {
                self.reply(prompt.request, Err(error));
                self.next();
            } else {
                self.start(prompt.request, prompt.scope, generation);
            }
        } else {
            drop(state);
            self.draw();
        }
        true
    }
}
