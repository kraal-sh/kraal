//! Form geometry and input shared by management screens.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};

use super::{overlays, ui};
use crate::ansi::{Cursor, CursorShape};
use crate::input::{InputEvent, KeyCode, MouseButton, MouseKind};

impl super::Server {
    pub(super) fn form_back(&self, client: super::ClientId, back: Option<ui::PickerState>) -> ui::Mode {
        let Some(state) = back else { return ui::Mode::Normal };
        let mut picker = match state.kind {
            ui::PickerKind::Accounts => self.accounts_picker(None),
            ui::PickerKind::Workspace => self.workspace_picker(self.clients.get(&client).map_or(0, |c| c.workspace)),
            ui::PickerKind::DeletedWorkspaces => self.deleted_workspace_picker(),
            ui::PickerKind::Pane => self.pane_picker(),
            ui::PickerKind::Ssh => self.ssh_picker(),
            ui::PickerKind::Tunnels => self.tunnel_picker(),
            _ => return ui::Mode::Normal,
        };
        picker.restore(state);
        ui::Mode::Picker(picker)
    }
}

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
    pub error_start: usize,
    pub max_scroll: usize,
    pub primary: Rect,
    pub alternate: Option<Rect>,
    pub cancel: Rect,
    pub footer: Rect,
}

impl FormLayout {
    pub fn new(screen: Rect, fields: usize, message: &str, error: Option<&str>, buttons: &Buttons<'_>) -> Self {
        Self::with_height(screen, fields, message, error, buttons, None)
    }

    pub fn with_height(
        screen: Rect,
        fields: usize,
        message: &str,
        error: Option<&str>,
        buttons: &Buttons<'_>,
        height: Option<u16>,
    ) -> Self {
        let width = screen.width.saturating_sub(4).clamp(4, 76);
        let wrap = |text: &str| {
            text.lines().flat_map(|l| crate::text::wrap(l, width.saturating_sub(4).max(1) as usize)).collect::<Vec<_>>()
        };
        let mut lines = wrap(message);
        let error_start = lines.len();
        lines.extend(wrap(error.unwrap_or("")));
        // A result with only Close uses the same action for Enter and Esc.
        let single = buttons.primary.field == buttons.cancel.field;
        let button_width =
            |button: &Button<'_>| (crate::text::columns(button.label) as u16 + 2).min(width.saturating_sub(4));
        let button_count = if single { 1 } else { 2 + u16::from(buttons.alternate.is_some()) };
        let total_width = button_width(&buttons.primary)
            + if single { 0 } else { button_width(&buttons.cancel) }
            + buttons.alternate.as_ref().map_or(0, button_width)
            + 3 * (button_count - 1);
        let stacked = total_width > width.saturating_sub(4);
        let base = fields as u16 + 5 + if stacked { button_count - 1 } else { 0 };
        let height = height.unwrap_or(lines.len() as u16 + base).min(screen.height.saturating_sub(2)).max(base);
        let area = overlays::centered(screen, width, height);
        let row = |y| Rect::new(area.x + 2, area.y + y, area.width.saturating_sub(4), 1);
        let body = Rect { height: area.height.saturating_sub(base), ..row(fields as u16 + 1) };
        let primary = Rect {
            width: button_width(&buttons.primary),
            ..row(area.height.saturating_sub(3 + if stacked { button_count - 1 } else { 0 }))
        };
        let next = |previous: Rect, button: &Button<'_>| Rect {
            width: button_width(button),
            ..if stacked {
                Rect { y: previous.y + 1, ..previous }
            } else {
                Rect { x: previous.right() + 3, ..previous }
            }
        };
        let alternate = buttons.alternate.as_ref().map(|b| next(primary, b));
        let cancel = if single { primary } else { next(alternate.unwrap_or(primary), &buttons.cancel) };
        Self {
            area,
            fields: (1..=fields).map(|y| row(y as u16)).collect(),
            body,
            max_scroll: lines.len().saturating_sub(body.height as usize),
            lines,
            error_start,
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
            .chain((buttons.primary.field != buttons.cancel.field).then_some(buttons.cancel.field))
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
    let (shown, before) = crate::text::input_window(&chars, cursor, room);
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
            Style::default().fg(if scroll + i >= layout.error_start {
                crate::theme::current().blocked
            } else {
                crate::theme::current().text
            }),
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
        buf.set_stringn(
            rect.x,
            rect.y,
            format!(" {} ", button.label),
            rect.width as usize,
            overlays::button_style(selected, false),
        );
    };
    button(buf, layout.cancel, &buttons.cancel);
    let mut keys = Vec::new();
    if pending.is_none() {
        if buttons.primary.field != buttons.cancel.field {
            button(buf, layout.primary, &buttons.primary);
        }
        if let Some((area, alternate)) = layout.alternate.zip(buttons.alternate.as_ref()) {
            button(buf, area, alternate);
        }
        keys.push(("Enter", verb));
        if buttons.primary.field != buttons.cancel.field {
            keys.push(("Tab", if layout.max_scroll > 0 || layout.fields.is_empty() { "Next" } else { "Next field" }));
        }
        if layout.max_scroll > 0 {
            keys.push(("PgUp/PgDn", "Scroll"));
        }
    }
    keys.push(("Esc", buttons.cancel.label));
    let line = overlays::key_line(&keys, "", layout.footer.width);
    buf.set_line(layout.footer.x, layout.footer.y, &line, layout.footer.width);
}

#[cfg(test)]
pub(super) fn assert_return(mode: ui::Mode, from: Option<&ui::Picker>) {
    if let Some(from) = from.filter(|p| !p.standalone_actions) {
        let ui::Mode::Picker(back) = mode else { panic!("expected the original list") };
        assert!(back.kind == from.kind);
        assert_eq!((&back.query, back.cursor), (&from.query, from.cursor));
        assert_eq!(back.current().map(|i| &i.label), from.current().map(|i| &i.label));
        if let Some(ui::PickAction::FocusPane { pane, .. }) = from.current().map(|i| &i.action) {
            assert!(
                matches!(back.current().map(|i| &i.action), Some(ui::PickAction::FocusPane { pane: p, .. }) if p == pane)
            );
        }
        assert!(back.actions.is_none());
    } else {
        assert!(matches!(mode, ui::Mode::Normal), "expected the panes");
    }
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
    fn footers_share_key_styles_and_keep_leaving_last() {
        let close = Buttons {
            primary: Button { field: 0, label: "Close" },
            alternate: None,
            cancel: Button { field: 0, label: "Close" },
        };
        for (buttons, fields, message, pending, width, expected) in [
            (buttons(false), 1, "", None, 80, "Enter Add · Tab Next field · Esc Cancel"),
            (buttons(false), 0, "", None, 80, "Enter Add · Tab Next · Esc Cancel"),
            (buttons(false), 1, "", Some(1), 80, "Esc Cancel"),
            (buttons(false), 1, "", None, 32, "Enter Add · Esc Cancel"),
            (buttons(false), 1, "line\nline\nline", None, 80, "Enter Add · Tab Next · PgUp/PgDn Scroll · Esc Cancel"),
            (close, 0, "line\nline\nline\nline", None, 80, "Enter Close · PgUp/PgDn Scroll · Esc Close"),
        ] {
            let screen = Rect::new(0, 0, width, 10);
            let layout = FormLayout::new(screen, fields, message, None, &buttons);
            let mut buf = Buffer::empty(screen);
            draw_buttons(&layout, &buttons, buttons.primary.field, pending, buttons.primary.label, &mut buf);
            let row: String =
                (layout.footer.x..layout.footer.right()).map(|x| buf[(x, layout.footer.y)].symbol()).collect();
            assert_eq!(row.trim_end(), expected);
            for key in ["Enter", "Tab", "PgUp/PgDn", "Esc"] {
                if let Some(at) = expected.find(key) {
                    let x = layout.footer.x + crate::text::columns(&expected[..at]) as u16;
                    for col in x..x + key.len() as u16 {
                        assert_eq!(buf[(col, layout.footer.y)].fg, crate::theme::current().accent);
                        assert!(buf[(col, layout.footer.y)].modifier.contains(Modifier::BOLD));
                    }
                    let action = &buf[(x + key.len() as u16 + 1, layout.footer.y)];
                    assert_eq!(action.fg, crate::theme::current().dim);
                    assert!(!action.modifier.contains(Modifier::BOLD));
                }
            }
        }
    }

    #[test]
    fn buttons_share_question_padding_spacing_focus_and_mouse_targets_when_stacked() {
        for width in [80, 124, 30, 16] {
            let screen = Rect::new(0, 0, width, 24);
            let buttons = buttons(true);
            let layout = FormLayout::new(screen, 1, "", None, &buttons);
            let areas = [
                (layout.primary, &buttons.primary),
                (layout.alternate.unwrap(), buttons.alternate.as_ref().unwrap()),
                (layout.cancel, &buttons.cancel),
            ];
            for selected in [0, 10, 11, 12] {
                let mut buf = Buffer::empty(screen);
                draw_buttons(&layout, &buttons, selected, None, "Add", &mut buf);
                for (area, button) in areas {
                    assert_eq!(area.width, (crate::text::columns(button.label) as u16 + 2).min(layout.body.width));
                    let label: String = format!(" {} ", button.label).chars().take(area.width as usize).collect();
                    let drawn: String = (area.x..area.right()).map(|x| buf[(x, area.y)].symbol()).collect();
                    assert_eq!(drawn, label);
                    let focused = selected == button.field || selected == 0 && button.field == 10;
                    for x in area.x..area.right() {
                        let style = overlays::button_style(focused, false);
                        let cell = &buf[(x, area.y)];
                        assert_eq!(cell.fg, style.fg.unwrap());
                        assert_eq!(cell.bg, style.bg.unwrap_or(ratatui::style::Color::Reset));
                        assert_eq!(cell.modifier, style.add_modifier);
                        let (mut field, mut scroll, mut error, mut pending) = (0, 0, None, None);
                        let outcome = FormState {
                            field: &mut field,
                            scroll: &mut scroll,
                            error: &mut error,
                            pending: &mut pending,
                        }
                        .handle(
                            mouse(MouseKind::Press(MouseButton::Left), Rect::new(x, area.y, 1, 1)),
                            &mut [],
                            &buttons,
                            &layout,
                        );
                        if button.field == buttons.cancel.field {
                            assert!(matches!(outcome, FormEvent::Cancel));
                        } else {
                            assert!(matches!(outcome, FormEvent::Submit));
                            assert_eq!(field, button.field);
                        }
                    }
                }
            }
            for pair in areas.windows(2) {
                let (previous, next) = (pair[0].0, pair[1].0);
                if width >= 80 {
                    assert_eq!(next.x, previous.right() + 3);
                    assert_eq!(next.y, previous.y);
                } else {
                    assert_eq!(next.x, previous.x);
                    assert_eq!(next.y, previous.y + 1);
                }
            }
        }
    }

    #[test]
    fn explanations_and_results_use_text_colour_and_only_errors_are_red_after_scrolling() {
        let message = "Starts in ~\nCommand uses sh syntax. Leave it empty for your shell.\nLayout exported";
        let error = "Cannot save. Check the folder and retry. ".repeat(30);
        for screen in [Rect::new(0, 0, 80, 24), Rect::new(0, 0, 124, 36)] {
            for (message, error) in [(message, None), (message, Some(error.as_str())), ("", Some(error.as_str()))] {
                let layout = FormLayout::new(screen, 1, message, error, &buttons(false));
                assert_eq!(layout.error_start == 0, message.is_empty());
                assert_eq!(layout.error_start == layout.lines.len(), error.is_none());
                for scroll in 0..=layout.max_scroll {
                    let mut buf = Buffer::empty(screen);
                    draw_message(&layout, scroll, &mut buf);
                    for row in 0..layout.body.height as usize {
                        let Some(line) = layout.lines.get(scroll + row) else { break };
                        let color = if scroll + row < layout.error_start {
                            crate::theme::current().text
                        } else {
                            crate::theme::current().blocked
                        };
                        for col in 0..crate::text::columns(line) {
                            assert_eq!(buf[(layout.body.x + col as u16, layout.body.y + row as u16)].fg, color);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fields_keep_unicode_input_and_buttons_use_the_same_keyboard_and_mouse_actions() {
        for alternate in [false, true] {
            let buttons = buttons(alternate);
            let layout = FormLayout::new(Rect::new(0, 0, 80, 24), 2, "", None, &buttons);
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
        let layout = FormLayout::new(Rect::new(0, 0, 80, 24), 1, "Checking...", None, &buttons);
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
            let layout = FormLayout::new(screen, 2, "", Some(&message), &buttons);
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
            let text = crate::testing::buffer_text(&buf);
            assert!(text.contains("final detail"));
            assert!(text.contains(" Add     Cancel "));
            assert_eq!(buf[layout.primary.as_position()].bg, crate::theme::current().accent);
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
