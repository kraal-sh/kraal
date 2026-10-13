//! Form geometry and input shared by management screens.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};

use super::{overlays, ui};
use crate::ansi::{Cursor, CursorShape};
use crate::input::{InputEvent, KeyCode, MouseButton, MouseKind};

pub(super) struct Button<'a> {
    pub field: usize,
    pub label: &'a str,
}

pub(super) struct Buttons<'a> {
    pub primary: Button<'a>,
    pub alternate: Option<Button<'a>>,
    pub cancel: Button<'a>,
}

pub(super) struct FormLayout {
    pub area: Rect,
    pub fields: Vec<Rect>,
    pub body: Rect,
    pub lines: Vec<String>,
    pub max_scroll: usize,
    pub primary: Rect,
    pub alternate: Option<Rect>,
    pub cancel: Rect,
    pub footer: Rect,
}

impl FormLayout {
    pub fn new(screen: Rect, fields: usize, message: &str, buttons: &Buttons<'_>) -> Self {
        let width = screen.width.saturating_sub(4).clamp(4, 76);
        let lines: Vec<_> =
            message.lines().flat_map(|l| crate::text::wrap(l, width.saturating_sub(4).max(1) as usize)).collect();
        let stacked = buttons.alternate.is_some();
        let base = fields as u16 + if stacked { 6 } else { 5 };
        let height = (lines.len() as u16 + base).min(screen.height.saturating_sub(2)).max(base);
        let area = overlays::centered(screen, width, height);
        let row = |y| Rect::new(area.x + 2, area.y + y, area.width.saturating_sub(4), 1);
        let body = Rect { height: area.height.saturating_sub(base), ..row(fields as u16 + 1) };
        let button_width = |button: &Button<'_>| crate::text::columns(button.label) as u16 + 4;
        let primary = Rect {
            width: button_width(&buttons.primary),
            ..row(area.height.saturating_sub(if stacked { 5 } else { 3 }))
        };
        let alternate =
            buttons.alternate.as_ref().map(|b| Rect { width: button_width(b), ..row(area.height.saturating_sub(4)) });
        let cancel = Rect {
            width: button_width(&buttons.cancel),
            ..if stacked { row(area.height.saturating_sub(3)) } else { Rect { x: primary.right() + 2, ..primary } }
        };
        Self {
            area,
            fields: (1..=fields).map(|y| row(y as u16)).collect(),
            body,
            max_scroll: lines.len().saturating_sub(body.height as usize),
            lines,
            primary,
            alternate,
            cancel,
            footer: row(area.height.saturating_sub(2)),
        }
    }
}

pub(super) enum FieldValue<'a> {
    Choice,
    Text { value: &'a mut String, cursor: &'a mut usize },
}

pub(super) struct Field<'a> {
    pub id: usize,
    pub area: Rect,
    pub value: FieldValue<'a>,
}

pub(super) enum FormEvent {
    None,
    Cancel,
    Submit,
    Choice { field: usize, keyboard: bool },
}

pub(super) struct FormState<'a> {
    pub field: &'a mut usize,
    pub scroll: &'a mut usize,
    pub error: &'a mut Option<String>,
    pub pending: &'a mut Option<u32>,
}

impl FormState<'_> {
    pub fn begin(&mut self, token: u32) -> bool {
        if self.pending.is_some() {
            return false;
        }
        *self.pending = Some(token);
        true
    }

    pub fn complete(&mut self, token: u32, error: Option<String>) -> bool {
        if *self.pending != Some(token) {
            return false;
        }
        *self.pending = None;
        *self.error = error;
        true
    }

    pub fn handle(
        &mut self,
        event: InputEvent,
        fields: &mut [Field<'_>],
        buttons: &Buttons<'_>,
        layout: &FormLayout,
    ) -> FormEvent {
        let cancel = match &event {
            InputEvent::Key(k) => k.code == KeyCode::Esc || k.mods.ctrl && matches!(k.code, KeyCode::Char('c' | 'g')),
            InputEvent::Mouse(m) => {
                matches!(m.kind, MouseKind::Press(MouseButton::Left))
                    && layout.cancel.contains(Position::new(m.col, m.row))
            }
            _ => false,
        };
        if cancel {
            return FormEvent::Cancel;
        }
        // The operation token also gates input, so queued clicks cannot submit the same form twice.
        if self.pending.is_some() {
            return FormEvent::None;
        }
        let button_ids: Vec<_> = std::iter::once(buttons.primary.field)
            .chain(buttons.alternate.as_ref().map(|b| b.field))
            .chain(std::iter::once(buttons.cancel.field))
            .collect();
        let move_to = |selected: &mut usize, ids: &[usize], backwards: bool| {
            let at = ids.iter().position(|id| id == selected).unwrap_or(0);
            *selected = ids[(at + if backwards { ids.len() - 1 } else { 1 }) % ids.len()];
        };
        match event {
            InputEvent::Key(k) => match k.code {
                KeyCode::Enter => {
                    return if *self.field == buttons.cancel.field { FormEvent::Cancel } else { FormEvent::Submit };
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let ids: Vec<_> = fields.iter().map(|f| f.id).chain(button_ids.iter().copied()).collect();
                    move_to(self.field, &ids, k.code == KeyCode::BackTab);
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                    if fields.iter().any(|f| f.id == *self.field && matches!(f.value, FieldValue::Choice)) =>
                {
                    return FormEvent::Choice { field: *self.field, keyboard: true };
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if button_ids.contains(self.field) => {
                    move_to(self.field, &button_ids, matches!(k.code, KeyCode::Left | KeyCode::Up));
                }
                KeyCode::PageDown => *self.scroll = (*self.scroll + layout.body.height as usize).min(layout.max_scroll),
                KeyCode::PageUp => *self.scroll = self.scroll.saturating_sub(layout.body.height as usize),
                _ => {
                    if let Some(Field { value: FieldValue::Text { value, cursor }, .. }) =
                        fields.iter_mut().find(|f| f.id == *self.field)
                        && ui::edit_line(value, cursor, &k)
                    {
                        *self.error = None;
                    }
                }
            },
            InputEvent::Paste(data) => {
                if let Some(Field { value: FieldValue::Text { value, cursor }, .. }) =
                    fields.iter_mut().find(|f| f.id == *self.field)
                {
                    *self.error = ui::paste_line(value, cursor, &String::from_utf8_lossy(&data)).err();
                }
            }
            InputEvent::Mouse(m) => match m.kind {
                MouseKind::Press(MouseButton::Left) => {
                    let at = Position::new(m.col, m.row);
                    if let Some(field) = fields.iter().find(|f| f.area.contains(at)) {
                        *self.field = field.id;
                        if matches!(field.value, FieldValue::Choice) {
                            return FormEvent::Choice { field: field.id, keyboard: false };
                        }
                    } else if layout.primary.contains(at) {
                        *self.field = buttons.primary.field;
                        return FormEvent::Submit;
                    } else if layout.alternate.is_some_and(|r| r.contains(at)) {
                        *self.field = buttons.alternate.as_ref().unwrap().field;
                        return FormEvent::Submit;
                    }
                }
                MouseKind::ScrollDown => *self.scroll = (*self.scroll + 2).min(layout.max_scroll),
                MouseKind::ScrollUp => *self.scroll = self.scroll.saturating_sub(2),
                _ => {}
            },
            _ => {}
        }
        FormEvent::None
    }
}

pub(super) fn field_style(selected: bool) -> Style {
    if selected {
        Style::default().bg(crate::theme::current().selection).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

pub(super) fn draw_text_field(
    area: Rect,
    label: &str,
    value: &str,
    cursor: usize,
    placeholder: &str,
    selected: bool,
    buf: &mut Buffer,
) -> Option<Cursor> {
    let label_width = crate::text::columns(label) as u16;
    let room = area.width.saturating_sub(label_width) as usize;
    let chars: Vec<_> = value.chars().collect();
    let (mut start, mut before) = (cursor.min(chars.len()), 0);
    while start > 0 {
        let width = unicode_width::UnicodeWidthChar::width(chars[start - 1]).unwrap_or(0);
        if before + width >= room {
            break;
        }
        before += width;
        start -= 1;
    }
    let shown: String = chars[start..].iter().collect();
    buf.set_stringn(area.x, area.y, format!("{label}{shown}"), area.width as usize, field_style(selected));
    if value.is_empty() {
        buf.set_stringn(
            area.x + label_width,
            area.y,
            placeholder,
            room,
            Style::default().fg(crate::theme::current().dim),
        );
    }
    (selected && room > 0).then_some(Cursor {
        x: area.x + label_width + before as u16,
        y: area.y,
        shape: CursorShape::Bar,
        blinking: true,
    })
}

pub(super) fn draw_message(layout: &FormLayout, scroll: usize, buf: &mut Buffer) {
    let scroll = scroll.min(layout.max_scroll);
    for (i, line) in layout.lines.iter().skip(scroll).take(layout.body.height as usize).enumerate() {
        buf.set_stringn(
            layout.body.x,
            layout.body.y + i as u16,
            line,
            layout.body.width as usize,
            Style::default().fg(crate::theme::current().blocked),
        );
    }
}

pub(super) fn draw_buttons(
    layout: &FormLayout,
    buttons: &Buttons<'_>,
    field: usize,
    pending: Option<u32>,
    verb: &str,
    buf: &mut Buffer,
) {
    let button = |buf: &mut Buffer, rect: Rect, button: &Button<'_>| {
        let selected = field == button.field || button.field == buttons.primary.field && field < button.field;
        let marker = if selected { "> " } else { "  " };
        buf.set_stringn(
            rect.x,
            rect.y,
            format!("[{marker}{}]", button.label),
            rect.width as usize,
            overlays::button_style(selected, false),
        );
    };
    button(buf, layout.cancel, &buttons.cancel);
    let footer = if pending.is_some() {
        "Esc Cancel".into()
    } else {
        button(buf, layout.primary, &buttons.primary);
        if let Some((area, alternate)) = layout.alternate.zip(buttons.alternate.as_ref()) {
            button(buf, area, alternate);
        }
        if layout.max_scroll > 0 {
            format!("Enter {verb} · Tab Next · PgUp/PgDn Scroll · Esc Cancel")
        } else {
            format!("Enter {verb} · Tab Next field · Esc Cancel")
        }
    };
    buf.set_stringn(
        layout.footer.x,
        layout.footer.y,
        footer,
        layout.footer.width as usize,
        Style::default().fg(crate::theme::current().dim),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, KeySpec, MouseEvent};

    fn key(spec: &str) -> InputEvent {
        let k = KeySpec::parse(spec).unwrap();
        InputEvent::Key(Key { code: k.code, mods: k.mods, raw: Vec::new() })
    }

    fn mouse(kind: MouseKind, area: Rect) -> InputEvent {
        InputEvent::Mouse(MouseEvent { kind, col: area.x, row: area.y, mods: Default::default() })
    }

    fn buttons(alternate: bool) -> Buttons<'static> {
        Buttons {
            primary: Button { field: 10, label: "Add" },
            alternate: alternate.then_some(Button { field: 11, label: "Use existing" }),
            cancel: Button { field: 12, label: "Cancel" },
        }
    }

    #[test]
    fn fields_keep_unicode_input_and_buttons_use_the_same_keyboard_and_mouse_actions() {
        for alternate in [false, true] {
            let buttons = buttons(alternate);
            let layout = FormLayout::new(Rect::new(0, 0, 80, 24), 2, "", &buttons);
            let (mut field, mut scroll, mut error, mut pending) = (1, 0, Some("Try again".into()), None);
            let mut state =
                FormState { field: &mut field, scroll: &mut scroll, error: &mut error, pending: &mut pending };
            let (mut name, mut cursor) = ("café 日".to_string(), 6);
            {
                let mut fields = [
                    Field { id: 0, area: layout.fields[0], value: FieldValue::Choice },
                    Field {
                        id: 1,
                        area: layout.fields[1],
                        value: FieldValue::Text { value: &mut name, cursor: &mut cursor },
                    },
                ];
                state.handle(key("left"), &mut fields, &buttons, &layout);
                state.handle(InputEvent::Paste("new ".as_bytes().into()), &mut fields, &buttons, &layout);
                assert!(state.error.is_none());
                state.handle(key("tab"), &mut fields, &buttons, &layout);
                assert_eq!(*state.field, 10);
                state.handle(key("right"), &mut fields, &buttons, &layout);
                assert_eq!(*state.field, if alternate { 11 } else { 12 });
                state.handle(key("shift+tab"), &mut fields, &buttons, &layout);
                assert_eq!(*state.field, 10);
                assert!(matches!(state.handle(key("enter"), &mut fields, &buttons, &layout), FormEvent::Submit));
                for (area, selected) in std::iter::once((layout.primary, 10)).chain(layout.alternate.map(|r| (r, 11))) {
                    assert!(matches!(
                        state.handle(mouse(MouseKind::Press(MouseButton::Left), area), &mut fields, &buttons, &layout),
                        FormEvent::Submit
                    ));
                    assert_eq!(*state.field, selected);
                }
                assert!(matches!(
                    state.handle(
                        mouse(MouseKind::Press(MouseButton::Left), layout.fields[0]),
                        &mut fields,
                        &buttons,
                        &layout
                    ),
                    FormEvent::Choice { field: 0, keyboard: false }
                ));
                assert!(matches!(
                    state.handle(key("right"), &mut fields, &buttons, &layout),
                    FormEvent::Choice { field: 0, keyboard: true }
                ));
                assert!(matches!(
                    state.handle(
                        mouse(MouseKind::Press(MouseButton::Left), layout.cancel),
                        &mut fields,
                        &buttons,
                        &layout
                    ),
                    FormEvent::Cancel
                ));
            }
            assert_eq!(name, "café new 日");
            assert_eq!(cursor, 9);
        }
    }

    #[test]
    fn a_pending_operation_accepts_only_cancel_and_a_result_must_match_its_token() {
        let buttons = buttons(true);
        let layout = FormLayout::new(Rect::new(0, 0, 80, 24), 1, "Checking...", &buttons);
        let (mut field, mut scroll, mut error, mut pending) = (0, 0, None, None);
        let mut state = FormState { field: &mut field, scroll: &mut scroll, error: &mut error, pending: &mut pending };
        let (mut value, mut cursor) = ("draft".to_string(), 5);
        let mut fields = [Field {
            id: 0,
            area: layout.fields[0],
            value: FieldValue::Text { value: &mut value, cursor: &mut cursor },
        }];
        assert!(state.begin(42));
        assert!(!state.begin(43));
        for event in [
            key("enter"),
            key("tab"),
            key("x"),
            InputEvent::Paste(b"ignored".to_vec()),
            mouse(MouseKind::Press(MouseButton::Left), layout.primary),
            mouse(MouseKind::Press(MouseButton::Left), layout.alternate.unwrap()),
            mouse(MouseKind::ScrollDown, layout.body),
        ] {
            assert!(matches!(state.handle(event, &mut fields, &buttons, &layout), FormEvent::None));
        }
        for event in
            [key("esc"), key("ctrl+c"), key("ctrl+g"), mouse(MouseKind::Press(MouseButton::Left), layout.cancel)]
        {
            assert!(matches!(state.handle(event, &mut fields, &buttons, &layout), FormEvent::Cancel));
        }
        assert!(!state.complete(43, Some("Stale error".into())));
        assert_eq!(*state.pending, Some(42));
        assert!(state.error.is_none());
        assert!(state.complete(42, Some("Try again".into())));
        assert_eq!(state.error.as_deref(), Some("Try again"));
        assert!(state.begin(44));
        assert!(!state.complete(42, None));
        assert_eq!(*state.pending, Some(44));
        assert!(state.complete(44, None));
        assert!(state.error.is_none());
        assert_eq!(value, "draft");
        assert_eq!(cursor, 5);
        assert_eq!(field, 0);
        assert_eq!(scroll, 0);
    }

    #[test]
    fn wrapped_errors_scroll_to_the_end_without_moving_fields_or_buttons() {
        for screen in [Rect::new(0, 0, 80, 24), Rect::new(0, 0, 124, 36)] {
            let buttons = buttons(false);
            let message = format!("{}final detail", "A long error with café and 日本語. ".repeat(150));
            let layout = FormLayout::new(screen, 2, &message, &buttons);
            assert!(layout.max_scroll > 0);
            assert!(layout.lines.iter().all(|l| crate::text::columns(l) <= layout.body.width as usize));
            let (mut field, mut scroll, mut error, mut pending) = (1, 0, Some(message), None);
            let mut state =
                FormState { field: &mut field, scroll: &mut scroll, error: &mut error, pending: &mut pending };
            while *state.scroll < layout.max_scroll {
                state.handle(key("pagedown"), &mut [], &buttons, &layout);
            }
            assert_eq!(*state.scroll, layout.max_scroll);
            let mut buf = Buffer::empty(screen);
            draw_message(&layout, *state.scroll, &mut buf);
            draw_buttons(&layout, &buttons, *state.field, *state.pending, "Add", &mut buf);
            let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
            assert!(text.contains("final detail"));
            assert!(text.contains("[> Add]  [  Cancel]"));
            assert!(text.contains("PgUp/PgDn Scroll"));
            state.handle(mouse(MouseKind::ScrollUp, layout.body), &mut [], &buttons, &layout);
            assert_eq!(*state.scroll, layout.max_scroll - 2);
            while *state.scroll > 0 {
                state.handle(key("pageup"), &mut [], &buttons, &layout);
            }
            assert_eq!(*state.field, 1);
        }
    }

    #[test]
    fn text_fields_keep_a_wide_character_cursor_visible() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
        let area = Rect::new(2, 3, 16, 1);
        let value = "a long café 日本語";
        let cursor = draw_text_field(area, "Name    ", value, value.chars().count(), "", true, &mut buf).unwrap();
        assert!(area.contains(Position::new(cursor.x, cursor.y)));
        assert_eq!(cursor.x, area.right() - 1);
        for (offset, ch) in [(6, "日"), (4, "本"), (2, "語")] {
            assert_eq!(buf[(cursor.x - offset, area.y)].symbol(), ch);
        }
        assert!(draw_text_field(area, "Name    ", value, 0, "", false, &mut buf).is_none());
    }
}
