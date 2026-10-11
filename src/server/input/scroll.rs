//! Scrolling a pane's history and searching it.

use alacritty_terminal::selection::SelectionType;

use crate::input::{InputEvent, Key, KeyCode};
use crate::layout::PaneId;
use crate::server::ui::Mode;
use crate::server::{ClientId, Server};

impl Server {
    pub(super) fn on_scroll(&mut self, id: ClientId, ev: InputEvent) -> Mode {
        let Some(p) = self.client_focus(id) else { return Mode::Normal };
        match ev {
            InputEvent::Key(k) => {
                let Some(pane) = self.panes.get_mut(&p) else { return Mode::Normal };
                if !pane.is_terminal() {
                    return self.scroll_other(id, p, &k);
                }
                pane.copy_mode(true);
                let page = pane.rows.max(2) as i32 - 1;
                let half = (pane.rows / 2).max(1) as i32;
                use alacritty_terminal::vi_mode::ViMotion as V;
                match (k.code, k.mods.ctrl) {
                    (KeyCode::Up, _) | (KeyCode::Char('k'), false) => pane.vi_motion(V::Up),
                    (KeyCode::Down, _) | (KeyCode::Char('j'), false) => pane.vi_motion(V::Down),
                    (KeyCode::Left, _) | (KeyCode::Char('h'), false) => pane.vi_motion(V::Left),
                    (KeyCode::Right, _) | (KeyCode::Char('l'), false) => pane.vi_motion(V::Right),
                    (KeyCode::Char('w'), false) => pane.vi_motion(V::SemanticRight),
                    (KeyCode::Char('b'), false) => pane.vi_motion(V::SemanticLeft),
                    (KeyCode::Char('e'), false) => pane.vi_motion(V::SemanticRightEnd),
                    (KeyCode::Char('W'), false) => pane.vi_motion(V::WordRight),
                    (KeyCode::Char('B'), false) => pane.vi_motion(V::WordLeft),
                    (KeyCode::Char('E'), false) => pane.vi_motion(V::WordRightEnd),
                    (KeyCode::Char('0'), false) | (KeyCode::Home, _) => pane.vi_motion(V::First),
                    (KeyCode::Char('^'), false) => pane.vi_motion(V::FirstOccupied),
                    (KeyCode::Char('$'), false) | (KeyCode::End, _) => pane.vi_motion(V::Last),
                    (KeyCode::Char('H'), false) => pane.vi_motion(V::High),
                    (KeyCode::Char('M'), false) => pane.vi_motion(V::Middle),
                    (KeyCode::Char('L'), false) => pane.vi_motion(V::Low),
                    (KeyCode::Char('{'), false) => pane.vi_motion(V::ParagraphUp),
                    (KeyCode::Char('}'), false) => pane.vi_motion(V::ParagraphDown),
                    (KeyCode::Char('%'), false) => pane.vi_motion(V::Bracket),
                    (KeyCode::Char('g'), false) => pane.vi_edge(true),
                    (KeyCode::Char('G'), false) => pane.vi_edge(false),
                    (KeyCode::PageUp, _) | (KeyCode::Char('b'), true) => pane.vi_scroll(page),
                    (KeyCode::PageDown, _) | (KeyCode::Char('f'), true) | (KeyCode::Char(' '), false) => {
                        pane.vi_scroll(-page)
                    }
                    (KeyCode::Char('u'), true) => pane.vi_scroll(half),
                    (KeyCode::Char('d'), true) => pane.vi_scroll(-half),
                    (KeyCode::Char('y'), true) => pane.scroll(1),
                    (KeyCode::Char('e'), true) => pane.scroll(-1),
                    (KeyCode::Char('v'), false) => pane.vi_select(SelectionType::Simple),
                    (KeyCode::Char('V'), false) => pane.vi_select(SelectionType::Lines),
                    (KeyCode::Char('v'), true) => pane.vi_select(SelectionType::Block),
                    (KeyCode::Char('y'), false) | (KeyCode::Enter, _) => {
                        if let Some(text) = pane.selection_text() {
                            self.copy_to_client(id, &text);
                        }
                        let pane = self.panes.get_mut(&p).unwrap();
                        pane.end_search();
                        pane.copy_mode(false);
                        return Mode::Normal;
                    }
                    (KeyCode::Char('/'), false) => {
                        return Mode::Search { query: String::new(), found: false };
                    }
                    (KeyCode::Char('n'), false) => {
                        pane.search_next(true);
                    }
                    (KeyCode::Char('N'), false) => {
                        pane.search_next(false);
                    }
                    (KeyCode::Esc, _) if pane.has_selection() => pane.clear_selection(),
                    (KeyCode::Esc, _) | (KeyCode::Char('q'), false) | (KeyCode::Char('c'), true) => {
                        pane.end_search();
                        pane.copy_mode(false);
                        return Mode::Normal;
                    }
                    _ => {}
                }
                Mode::Scroll
            }
            InputEvent::Mouse(m) => self.on_mouse(id, m, true).unwrap_or(Mode::Scroll),
            _ => Mode::Scroll,
        }
    }

    /// Scrolling a preview or file manager pane (no cursor there).
    pub(super) fn scroll_other(&mut self, _id: ClientId, p: PaneId, k: &Key) -> Mode {
        let Some(pane) = self.panes.get_mut(&p) else { return Mode::Normal };
        let half = (pane.rows / 2).max(1) as i32;
        match (k.code, k.mods.ctrl) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), false) => pane.scroll(1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), false) => pane.scroll(-1),
            (KeyCode::PageUp, _) | (KeyCode::Char('b'), true) => pane.scroll_page(true),
            (KeyCode::PageDown, _) | (KeyCode::Char(' '), false) | (KeyCode::Char('f'), true) => {
                pane.scroll_page(false)
            }
            (KeyCode::Char('u'), true) => pane.scroll(half),
            (KeyCode::Char('d'), true) => pane.scroll(-half),
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => pane.scroll_to_top(),
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => pane.scroll_to_bottom(),
            (KeyCode::Esc, _) | (KeyCode::Char('q'), false) => return Mode::Normal,
            _ => {}
        }
        Mode::Scroll
    }

    pub(super) fn on_search(&mut self, id: ClientId, mut query: String, mut found: bool, ev: InputEvent) -> Mode {
        let Some(p) = self.client_focus(id) else { return Mode::Normal };
        let Some(pane) = self.panes.get_mut(&p) else { return Mode::Normal };
        match ev {
            InputEvent::Key(k) => match (k.code, k.mods.ctrl) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), true) | (KeyCode::Char('g'), true) => {
                    pane.end_search();
                    pane.clear_selection();
                    return Mode::Scroll;
                }
                (KeyCode::Enter, _) => return Mode::Scroll,
                (KeyCode::Char('n'), true) | (KeyCode::Up, _) => found = pane.search_next(true),
                (KeyCode::Char('p'), true) | (KeyCode::Down, _) => found = pane.search_next(false),
                (KeyCode::Backspace, _) => {
                    query.pop();
                    found = pane.search(&query);
                }
                (KeyCode::Char('u'), true) => {
                    query.clear();
                    found = pane.search(&query);
                }
                (KeyCode::Char(ch), false) if !k.mods.alt => {
                    query.push(ch);
                    found = pane.search(&query);
                }
                _ => {}
            },
            InputEvent::Paste(data) => {
                query.push_str(String::from_utf8_lossy(&data).lines().next().unwrap_or(""));
                found = pane.search(&query);
            }
            InputEvent::Mouse(m) => {
                if let Some(next) = self.on_mouse(id, m, true) {
                    return next;
                }
            }
            _ => {}
        }
        Mode::Search { query, found }
    }
}
