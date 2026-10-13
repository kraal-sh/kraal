//! Account management shared by the list, sidebar and add form.

use anyhow::{Context, Result};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};

use super::ui::{Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Server, overlays};
use crate::account_status::{self, Status};
use crate::accounts::{self, Account, Accounts, Usage};
use crate::ansi::{Cursor, CursorShape};
use crate::input::{InputEvent, KeyCode, MouseButton, MouseKind};
use crate::layout::PaneId;
use crate::text::wrap;

#[derive(Clone)]
pub enum AccountChoice {
    New,
    Existing,
}

#[derive(Clone)]
pub struct AccountForm {
    pub agent: usize,
    pub name: String,
    pub cursor: usize,
    pub field: usize,
    pub existing: Option<Account>,
    pub choice: Option<AccountChoice>,
    pub installed: bool,
    pub error: Option<String>,
    pub scroll: usize,
    pub checking: Option<u32>,
}

impl AccountForm {
    fn choosing(&self) -> bool {
        self.existing.is_some() && self.choice.is_none()
    }
}

pub struct AccountDetails {
    pub account: String,
    pub title: String,
    pub text: String,
    pub scroll: usize,
}

pub(super) fn agent_name(agent: &str) -> String {
    let mut name = agent.to_string();
    if let Some(first) = name.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    name
}

pub(super) fn label(a: &Account) -> String {
    format!("{} · {}", agent_name(&a.agent), a.name)
}

fn age(observed: i64, now: i64) -> String {
    let seconds = now.saturating_sub(observed).max(0);
    if seconds < 60 {
        "just now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

pub(super) fn usage_summary(a: &Account, usage: Option<&Usage>, now: i64) -> String {
    if a.sign_in_required {
        return "Sign-in required".into();
    }
    let Some(u) = usage else { return "Usage unavailable".into() };
    if u.five_hour.is_none() && u.weekly.is_none() {
        return "Usage unavailable".into();
    }
    let mut live = [&u.five_hour, &u.weekly]
        .into_iter()
        .zip([5 * 3600, 7 * 86400])
        .filter_map(|(w, period)| {
            w.as_ref().filter(|w| w.resets_at.unwrap_or(u.observed_at.saturating_add(period)) > now)
        })
        .peekable();
    if live.peek().is_some() {
        format!("{:.0}% used", live.map(|w| w.used_percent).fold(0.0, f64::max))
    } else if u.observed_at > 0 {
        format!("Last checked {}", age(u.observed_at, now))
    } else {
        "Usage unavailable".into()
    }
}

pub(super) fn sidebar_summary(summary: &str, width: usize) -> String {
    if crate::text::columns(summary) <= width {
        return summary.into();
    }
    let short = summary
        .replace("Usage unavailable", "No usage")
        .replace("Last checked", "Checked")
        .replace("Sign-in required", "Sign in")
        .replace("Could not save · Check again", "Retry save")
        .replace("Could not verify · Check again", "Retry check")
        .replace("Checking sign-in", "Checking")
        .replace("% used", "%");
    if crate::text::columns(&short) <= width {
        return short;
    }
    let status = short.split(" · ").next().unwrap_or_default();
    if crate::text::columns(status) <= width { status.into() } else { String::new() }
}

impl Server {
    pub(super) fn account_summary(&self, a: &Account) -> String {
        self.account_summary_at(a, accounts::now())
    }

    fn account_summary_at(&self, a: &Account, now: i64) -> String {
        let summary = usage_summary(a, self.usage.get(a), now);
        if summary == "Sign-in required" || summary.starts_with("Last checked") {
            summary
        } else {
            let count = self.panes.keys().filter(|&&p| self.pane_account(p).is_some_and(|b| b.id == a.id)).count();
            if count == 0 { summary } else { format!("{summary} · {count} pane{}", if count == 1 { "" } else { "s" }) }
        }
    }

    pub(super) fn accounts_picker(&self, selected: Option<&str>) -> Picker {
        self.accounts_picker_at(selected, accounts::now())
    }

    pub(super) fn accounts_picker_at(&self, selected: Option<&str>, now: i64) -> Picker {
        let mut items: Vec<_> = self
            .accounts
            .all()
            .iter()
            .map(|a| PickItem::new(label(a), self.account_summary_at(a, now), PickAction::AccountDetails(a.id.clone())))
            .collect();
        items.extend(
            self.accounts
                .reviews()
                .into_iter()
                .map(|a| PickItem::new(accounts::existing_label(&a.agent), "Review", PickAction::ReviewAccount(a))),
        );
        for a in self.accounts.unlisted() {
            use super::agents::ExistingCheck;
            let detail = match self.existing_checks.get(&a.agent) {
                Some(ExistingCheck::Checking) => "Checking sign-in",
                Some(ExistingCheck::SaveFailed) => "Could not save · Check again",
                Some(ExistingCheck::Checked(Status::CouldNotCheck)) => "Could not verify · Check again",
                _ => continue,
            };
            items.push(PickItem::new(
                accounts::existing_label(&a.agent),
                detail,
                PickAction::CheckExisting { agent: a.agent },
            ));
        }
        items.push(PickItem::new("+ Add account", "", PickAction::AddAccount));
        let mut p = Picker::new(PickerKind::Accounts, "Accounts", items);
        if let Some(id) = selected {
            p.selected =
                p.items.iter().position(|i| matches!(&i.action, PickAction::AccountDetails(a) if a == id)).unwrap_or(0);
        }
        p
    }

    /// Account rows hold until a usage window resets or a "Last checked" age changes.
    pub(super) fn account_summaries_until(&self, now: i64) -> Option<i64> {
        let ages = self.usage.0.values().filter_map(|u| {
            if u.observed_at <= 0 || u.next_end(now).is_some() || (u.five_hour.is_none() && u.weekly.is_none()) {
                return None;
            }
            let seconds = now.saturating_sub(u.observed_at).max(0);
            let step = if seconds < 3600 {
                60
            } else if seconds < 86400 {
                3600
            } else {
                86400
            };
            now.checked_add(step - seconds % step)
        });
        self.usage.next_end(now).into_iter().chain(ages).min()
    }

    pub(super) fn account_details(&self, id: &str) -> Mode {
        let Some(a) = self.accounts.by_id(id) else { return Mode::Picker(self.accounts_picker(None)) };
        let now = accounts::now();
        let mut text = self.account_summary(&a);
        if let Some(u) = self.usage.get(&a) {
            if u.observed_at > 0 {
                text.push_str(&format!("\nLast checked {}", age(u.observed_at, now)));
            }
            for (name, w) in [("5-hour", &u.five_hour), ("Weekly", &u.weekly)] {
                let detail = match w {
                    Some(w) => {
                        let reset = accounts::describe_reset(w.resets_at, now);
                        format!(
                            "{:.0}% used · {}",
                            w.used_percent,
                            if reset.is_empty() { "Reset unavailable" } else { &reset }
                        )
                    }
                    None => "Usage unavailable".into(),
                };
                text.push_str(&format!("\n{name}: {detail}"));
            }
        }
        Mode::AccountDetails(AccountDetails { account: a.id.clone(), title: label(&a), text, scroll: 0 })
    }

    pub(super) fn account_form(&self, client: ClientId) -> AccountForm {
        let agent = self
            .client_focus(client)
            .and_then(|p| {
                self.pane_account(p).map(|a| a.agent).or_else(|| self.panes.get(&p).and_then(|p| p.agent.kind.clone()))
            })
            .filter(|a| accounts::agent_kind(a).is_some())
            .or_else(|| self.clients.get(&client).and_then(|c| c.last_account_agent.clone()));
        let agent = accounts::AGENTS.iter().position(|k| Some(k.name) == agent.as_deref()).unwrap_or(0);
        let mut form = AccountForm {
            agent,
            name: String::new(),
            cursor: 0,
            field: 1,
            existing: None,
            choice: None,
            installed: false,
            error: None,
            scroll: 0,
            checking: None,
        };
        self.fill_account_form(&mut form);
        form
    }

    pub(super) fn fill_account_form(&self, form: &mut AccountForm) {
        let choosing = form.choosing();
        let kind = accounts::AGENTS[form.agent];
        form.installed = crate::transfer::have_program(kind.command);
        if matches!(form.choice, Some(AccountChoice::Existing)) {
            return;
        }
        form.existing = self.accounts.reviews().into_iter().chain(self.accounts.unlisted()).find(|a| {
            a.agent == kind.name
                && (a.review
                    || matches!(
                        self.existing_checks.get(&a.agent),
                        Some(
                            super::agents::ExistingCheck::Checked(Status::SignedIn)
                                | super::agents::ExistingCheck::SaveFailed
                        )
                    ))
        });
        if form.choosing() && !choosing {
            form.field = 2;
        } else if !form.choosing() && choosing {
            form.field = 1;
        }
    }

    pub(super) fn name_existing_form(&self, client: ClientId, existing: Account) -> AccountForm {
        let mut form = self.account_form(client);
        form.agent = accounts::AGENTS.iter().position(|k| k.name == existing.agent).unwrap_or(0);
        form.installed = crate::transfer::have_program(accounts::AGENTS[form.agent].command);
        form.name = self.accounts.proposed_name(&existing.agent, existing.public_identity.as_deref());
        form.cursor = form.name.chars().count();
        form.existing = Some(existing);
        form.choice = Some(AccountChoice::Existing);
        form.field = 1;
        form
    }

    fn account_added(&mut self, client: ClientId, account: &Account) {
        let mut picker = self.accounts_picker(Some(&account.id));
        picker.message = Some("Account added".into());
        if let Some(c) = self.clients.get_mut(&client) {
            c.mode = Mode::Picker(picker);
        }
        self.mark_dirty();
    }

    fn use_existing_form(&mut self, client: ClientId, mut form: AccountForm) -> Mode {
        let existing = form.existing.clone().unwrap();
        let checked = existing.clone();
        let name = form.name.trim().to_string();
        let token = self.alloc_id();
        form.checking = Some(token);
        let saved_form = form.clone();
        if !self.background(
            move || account_status::inspect(&checked),
            move |server, status| {
                if !server
                    .clients
                    .get(&client)
                    .is_some_and(|c| matches!(&c.mode, Mode::AccountForm(f) if f.checking == Some(token)))
                {
                    return;
                }
                let result = Accounts::update(|all| name_existing_account(all, &existing, &status, &name));
                server.reload_accounts();
                match result {
                    Ok(a) => server.account_added(client, &a),
                    Err(e) => {
                        let mut form = saved_form;
                        form.checking = None;
                        form.error = Some(format!("{e:#}"));
                        server.clients.get_mut(&client).unwrap().mode = Mode::AccountForm(form);
                        server.mark_dirty();
                    }
                }
            },
        ) {
            form.checking = None;
            form.error = Some("Could not start the sign-in check. Try again.".into());
        }
        Mode::AccountForm(form)
    }

    pub(super) fn on_account_form(&mut self, client: ClientId, mut form: AccountForm, event: InputEvent) -> Mode {
        let layout = form_layout(&form, self.client_screen(client));
        if form.checking.is_some() {
            let cancel = match &event {
                InputEvent::Key(k) => {
                    k.code == KeyCode::Esc || (k.mods.ctrl && matches!(k.code, KeyCode::Char('c' | 'g')))
                }
                InputEvent::Mouse(m) => {
                    matches!(m.kind, MouseKind::Press(MouseButton::Left))
                        && layout.cancel.contains(Position::new(m.col, m.row))
                }
                _ => false,
            };
            return if cancel { Mode::Picker(self.accounts_picker(None)) } else { Mode::AccountForm(form) };
        }
        let mut submit = false;
        let mut cancel = false;
        match event {
            InputEvent::Key(k) => match k.code {
                KeyCode::Esc => cancel = true,
                KeyCode::Char('c' | 'g') if k.mods.ctrl => cancel = true,
                KeyCode::Enter => {
                    cancel = form.field == 4;
                    submit = !cancel;
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let fields: &[usize] = if form.choosing() { &[0, 2, 3, 4] } else { &[0, 1, 2, 4] };
                    let at = fields.iter().position(|f| *f == form.field).unwrap_or(0);
                    let next = if k.code == KeyCode::BackTab { at + fields.len() - 1 } else { at + 1 };
                    form.field = fields[next % fields.len()];
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if form.field == 0 => {
                    form.agent = (form.agent + 1) % accounts::AGENTS.len();
                    form.choice = None;
                    self.fill_account_form(&mut form);
                    form.error = None;
                    form.scroll = 0;
                }
                KeyCode::Left | KeyCode::Up | KeyCode::Right | KeyCode::Down if form.field >= 2 => {
                    let fields: &[usize] = if form.choosing() { &[2, 3, 4] } else { &[2, 4] };
                    let at = fields.iter().position(|f| *f == form.field).unwrap_or(0);
                    let next =
                        if matches!(k.code, KeyCode::Left | KeyCode::Up) { at + fields.len() - 1 } else { at + 1 };
                    form.field = fields[next % fields.len()];
                }
                KeyCode::PageDown => form.scroll = (form.scroll + layout.body.height as usize).min(layout.max_scroll),
                KeyCode::PageUp => form.scroll = form.scroll.saturating_sub(layout.body.height as usize),
                _ if form.field == 1 && super::ui::edit_line(&mut form.name, &mut form.cursor, &k) => {
                    form.error = None;
                }
                _ => {}
            },
            InputEvent::Paste(data) if form.field == 1 => {
                form.error =
                    super::ui::paste_line(&mut form.name, &mut form.cursor, &String::from_utf8_lossy(&data)).err();
            }
            InputEvent::Mouse(m) => match m.kind {
                MouseKind::Press(MouseButton::Left) => {
                    let at = Position::new(m.col, m.row);
                    if layout.agent.contains(at) {
                        form.field = 0;
                        form.agent = (form.agent + 1) % accounts::AGENTS.len();
                        form.choice = None;
                        self.fill_account_form(&mut form);
                    } else if !form.choosing() && layout.name.contains(at) {
                        form.field = 1;
                    } else if layout.primary.contains(at) {
                        form.field = 2;
                        submit = true;
                    } else if layout.alternate.is_some_and(|r| r.contains(at)) {
                        form.field = 3;
                        submit = true;
                    } else if layout.cancel.contains(at) {
                        cancel = true;
                    }
                }
                MouseKind::ScrollDown => form.scroll = (form.scroll + 2).min(layout.max_scroll),
                MouseKind::ScrollUp => form.scroll = form.scroll.saturating_sub(2),
                _ => {}
            },
            _ => {}
        }
        if let Some(c) = self.clients.get_mut(&client) {
            c.last_account_agent = Some(accounts::AGENTS[form.agent].name.into());
        }
        if cancel {
            return Mode::Picker(self.accounts_picker(None));
        }
        if submit {
            if form.choosing() {
                if form.field == 3 {
                    form.choice = Some(AccountChoice::New);
                    form.field = 1;
                    form.error = None;
                    return Mode::AccountForm(form);
                }
                let existing = form.existing.as_ref().unwrap();
                if existing.review {
                    return Mode::Picker(self.review_account_picker(existing));
                }
                return Mode::AccountForm(self.name_existing_form(client, existing.clone()));
            }
            form.installed = crate::transfer::have_program(accounts::AGENTS[form.agent].command);
            form.scroll = 0;
            if !form.installed {
                form.error = Some(install_message(form.agent));
            } else if form.name.trim().is_empty() {
                form.error = Some("Enter a name for this account.".into());
                form.field = 1;
            } else if matches!(form.choice, Some(AccountChoice::Existing)) {
                return self.use_existing_form(client, form);
            } else {
                match self.add_account(client, accounts::AGENTS[form.agent].name, form.name.trim()) {
                    Ok(pane) => {
                        if let Some(c) = self.clients.get_mut(&client) {
                            c.adding_account = Some(pane);
                        }
                        return Mode::Normal;
                    }
                    Err(e) => form.error = Some(format!("{e:#}")),
                }
            }
        }
        Mode::AccountForm(form)
    }

    pub(super) fn on_account_details(
        &mut self,
        client: ClientId,
        mut details: AccountDetails,
        event: InputEvent,
    ) -> Mode {
        let area = details_area(self.client_screen(client));
        let footer_y = area.bottom().saturating_sub(2);
        let mut actions = false;
        let back = match event {
            InputEvent::Key(k) => match k.code {
                KeyCode::Esc | KeyCode::Enter => true,
                KeyCode::Tab => {
                    actions = true;
                    false
                }
                KeyCode::Down | KeyCode::PageDown => {
                    details.scroll = details.scroll.saturating_add(1);
                    false
                }
                KeyCode::Up | KeyCode::PageUp => {
                    details.scroll = details.scroll.saturating_sub(1);
                    false
                }
                _ => false,
            },
            InputEvent::Mouse(m) => match m.kind {
                MouseKind::ScrollDown => {
                    details.scroll = details.scroll.saturating_add(1);
                    false
                }
                MouseKind::ScrollUp => {
                    details.scroll = details.scroll.saturating_sub(1);
                    false
                }
                MouseKind::Press(MouseButton::Left) if m.row == footer_y => {
                    actions = m.col >= area.x + 15 && m.col < area.x + 26;
                    (m.col >= area.x + 2 && m.col < area.x + 12) || (m.col >= area.x + 28 && m.col < area.x + 36)
                }
                _ => false,
            },
            _ => false,
        };
        if actions {
            let mut picker = self.accounts_picker(Some(&details.account));
            if matches!(picker.current().map(|i| &i.action), Some(PickAction::AccountDetails(_))) {
                picker.actions = Some(0);
            }
            Mode::Picker(picker)
        } else if back {
            Mode::Picker(self.accounts_picker(Some(&details.account)))
        } else {
            Mode::AccountDetails(details)
        }
    }

    pub(super) fn added_login_finished(&mut self, pane: PaneId, account: &Account) {
        let clients: Vec<_> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                (c.adding_account == Some(pane)
                    && matches!(c.mode, Mode::Normal)
                    && self.client_focus(id) == Some(pane))
                .then_some(id)
            })
            .collect();
        self.abandon_added_login(pane);
        for id in clients {
            self.account_added(id, account);
        }
    }

    pub(super) fn abandon_added_login(&mut self, pane: PaneId) {
        for c in self.clients.values_mut() {
            if c.adding_account == Some(pane) {
                c.adding_account = None;
            }
        }
    }
}

/// Called under the accounts lock, after the sign-in check completes.
fn name_existing_account(
    all: &mut Accounts,
    existing: &Account,
    checked: &account_status::Checked,
    name: &str,
) -> Result<Account> {
    anyhow::ensure!(
        checked.status == Status::SignedIn || existing.review && checked.status == Status::NotSignedIn,
        "{}. Try Use existing account again.",
        checked.status.label()
    );
    let pending = if existing.review { all.reviews() } else { all.unlisted() };
    anyhow::ensure!(pending.contains(existing), "The existing account changed. Open Add account again.");
    let a = if existing.review {
        all.review(existing, None)?
    } else {
        all.adopt_checked(existing, checked.identity.as_deref())?;
        let a = all.by_id(&existing.id).context("The existing account changed. Try again.")?;
        if a.review { all.review(&a, None)? } else { a }
    };
    if a.name != name {
        all.rename(&a.agent, &a.key(), name)?;
    }
    all.set_sign_in_required(&a, checked.status == Status::NotSignedIn);
    all.by_id(&a.id).context("The account changed. Try again.")
}

fn install_message(agent: usize) -> String {
    let kind = accounts::AGENTS[agent];
    format!(
        "{} is not installed. Install {} and make `{}` available on PATH, then try again.",
        agent_name(kind.name),
        agent_name(kind.name),
        kind.command
    )
}

pub(super) struct FormLayout {
    area: Rect,
    agent: Rect,
    name: Rect,
    body: Rect,
    lines: Vec<String>,
    max_scroll: usize,
    primary: Rect,
    alternate: Option<Rect>,
    cancel: Rect,
    footer: Rect,
}

fn form_layout(form: &AccountForm, screen: Rect) -> FormLayout {
    let width = screen.width.saturating_sub(4).clamp(4, 76);
    let message = if form.checking.is_some() {
        "Checking sign-in...".into()
    } else {
        form.error.clone().unwrap_or_else(|| if form.installed { String::new() } else { install_message(form.agent) })
    };
    let lines: Vec<_> = message.lines().flat_map(|l| wrap(l, width.saturating_sub(4).max(1) as usize)).collect();
    let base = if form.choosing() { 8 } else { 7 };
    let height = (lines.len() as u16 + base).min(screen.height.saturating_sub(2)).max(base);
    let area = overlays::centered(screen, width, height);
    let row = |y| Rect::new(area.x + 2, area.y + y, area.width.saturating_sub(4), 1);
    let body = Rect { height: area.height.saturating_sub(base), ..row(3) };
    let primary = row(area.height.saturating_sub(if form.choosing() { 5 } else { 3 }));
    let primary_width = if form.choosing() || matches!(form.choice, Some(AccountChoice::Existing)) { 24 } else { 11 };
    let alternate = form.choosing().then(|| Rect { width: 30, ..row(area.height.saturating_sub(4)) });
    let cancel = if form.choosing() {
        Rect { width: 10, ..row(area.height.saturating_sub(3)) }
    } else {
        Rect { x: primary.x + primary_width + 2, width: 10, ..primary }
    };
    FormLayout {
        area,
        agent: row(1),
        name: row(2),
        body,
        max_scroll: lines.len().saturating_sub(body.height as usize),
        lines,
        primary: Rect { width: primary_width, ..primary },
        alternate,
        cancel,
        footer: row(area.height.saturating_sub(2)),
    }
}

pub(super) fn draw_form(form: &AccountForm, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let l = form_layout(form, screen);
    if l.area.width < 20 || l.area.height < 7 {
        return None;
    }
    let theme = crate::theme::current();
    overlays::frame(l.area, "Add account", theme.accent, buf);
    let style = |field| {
        if form.field == field {
            Style::default().bg(theme.selection).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    };
    let mut agent = format!("Agent   {}  (Left/Right)", agent_name(accounts::AGENTS[form.agent].name));
    if !form.installed {
        agent.push_str(" · Not installed");
    }
    buf.set_stringn(l.agent.x, l.agent.y, agent, l.agent.width as usize, style(0));
    let room = l.name.width.saturating_sub(8) as usize;
    let chars: Vec<_> = form.name.chars().collect();
    let (mut start, mut before) = (form.cursor.min(chars.len()), 0);
    while start > 0 {
        let width = unicode_width::UnicodeWidthChar::width(chars[start - 1]).unwrap_or(0);
        if before + width >= room {
            break;
        }
        before += width;
        start -= 1;
    }
    let shown: String = chars[start..].iter().collect();
    if form.choosing() {
        let message = format!("Existing {} sign-in needs review.", agent_name(accounts::AGENTS[form.agent].name));
        buf.set_stringn(l.name.x, l.name.y, message, l.name.width as usize, Style::default());
    } else {
        buf.set_stringn(l.name.x, l.name.y, format!("Name    {shown}"), l.name.width as usize, style(1));
        if form.name.is_empty() {
            buf.set_stringn(l.name.x + 8, l.name.y, "e.g. Work or Personal", room, Style::default().fg(theme.dim));
        }
    }
    let scroll = form.scroll.min(l.max_scroll);
    for (i, line) in l.lines.iter().skip(scroll).take(l.body.height as usize).enumerate() {
        buf.set_stringn(l.body.x, l.body.y + i as u16, line, l.body.width as usize, Style::default().fg(theme.blocked));
    }
    let primary = if form.choosing() || matches!(form.choice, Some(AccountChoice::Existing)) {
        "Use existing account"
    } else {
        "Sign in"
    };
    let button = |buf: &mut Buffer, rect: Rect, label: &str, field| {
        let selected = form.field == field || field == 2 && form.field < 2;
        let marker = if selected { "> " } else { "  " };
        buf.set_stringn(
            rect.x,
            rect.y,
            format!("[{marker}{label}]"),
            rect.width as usize,
            overlays::button_style(selected, false),
        );
    };
    button(buf, l.cancel, "Cancel", 4);
    if form.checking.is_some() {
        buf.set_stringn(l.footer.x, l.footer.y, "Esc Cancel", l.footer.width as usize, Style::default().fg(theme.dim));
        return None;
    }
    button(buf, l.primary, primary, 2);
    if let Some(r) = l.alternate {
        button(buf, r, "Sign in to another account", 3);
    }
    let verb = match form.field {
        3 => "Choose",
        4 => "Cancel",
        _ if form.choosing() => "Review",
        _ => primary,
    };
    let footer = if l.max_scroll > 0 {
        format!("Enter {verb} · Tab Next · PgUp/PgDn Scroll · Esc Cancel")
    } else {
        format!("Enter {verb} · Tab Next field · Esc Cancel")
    };
    buf.set_stringn(l.footer.x, l.footer.y, footer, l.footer.width as usize, Style::default().fg(theme.dim));
    (form.field == 1 && !form.choosing() && form.checking.is_none() && room > 0).then_some(Cursor {
        x: l.name.x + 8 + before as u16,
        y: l.name.y,
        shape: CursorShape::Bar,
        blinking: true,
    })
}

fn details_area(screen: Rect) -> Rect {
    overlays::centered(screen, screen.width.saturating_sub(4).min(76), screen.height.saturating_sub(4).min(14))
}

pub(super) fn draw_details(details: &AccountDetails, screen: Rect, buf: &mut Buffer) {
    let area = details_area(screen);
    let inner = overlays::frame(area, &details.title, crate::theme::current().accent, buf);
    let width = inner.width.saturating_sub(2);
    let lines: Vec<_> = details.text.lines().flat_map(|l| wrap(l, width.max(1) as usize)).collect();
    let height = inner.height.saturating_sub(2) as usize;
    let start = details.scroll.min(lines.len().saturating_sub(height));
    for (i, line) in lines.iter().skip(start).take(height).enumerate() {
        buf.set_stringn(inner.x + 1, inner.y + i as u16, line, width as usize, Style::default());
    }
    if inner.height > 0 {
        buf.set_stringn(
            inner.x + 1,
            inner.y + inner.height - 1,
            "Enter Back · Tab Actions · Esc Back",
            width as usize,
            Style::default(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Window;
    use crate::input::{Key, KeySpec, MouseEvent};

    fn account() -> Account {
        Account {
            id: accounts::new_uuid(),
            agent: "claude".into(),
            name: "work".into(),
            home: "/nonexistent/work".into(),
            threshold: None,
            note: None,
            sign_in_required: false,
            review: false,
            public_identity: None,
            linked_to: None,
            references: Vec::new(),
        }
    }

    fn server() -> Server {
        let (tx, _) = super::super::queue::channel(16);
        super::super::tests::server(tx)
    }

    fn key(spec: &str) -> InputEvent {
        let k = KeySpec::parse(spec).unwrap();
        InputEvent::Key(Key { code: k.code, mods: k.mods, raw: Vec::new() })
    }

    #[test]
    fn delayed_added_login_preserves_drafts_and_only_returns_from_its_own_pane() {
        use super::super::ui::{Prompt, PromptAction};

        let mut server = server();
        let (client, _peer) = super::super::tests::client(&mut server);
        let a = server.accounts.add(account()).unwrap();
        let login = server.alloc_id();
        server.workspaces[0].tab_mut().focus = Some(login);
        server.clients.get_mut(&client).unwrap().adding_account = Some(login);
        server.clients.get_mut(&client).unwrap().mode =
            Mode::Prompt(Prompt::new("Rename workspace", "", PromptAction::RenameWorkspace(server.workspaces[0].id)));
        server.client_input(client, b"unfinished draft");
        server.added_login_finished(login, &a);
        let Mode::Prompt(prompt) = &server.clients[&client].mode else { panic!("draft discarded") };
        assert_eq!(prompt.value, "unfinished draft");
        assert_eq!(prompt.cursor, "unfinished draft".len());
        assert_eq!(server.clients[&client].adding_account, None);

        let mut form = server.account_form(client);
        form.name = "another account draft".into();
        server.clients.get_mut(&client).unwrap().mode = Mode::AccountForm(form);
        server.clients.get_mut(&client).unwrap().adding_account = Some(login);
        server.added_login_finished(login, &a);
        assert!(matches!(&server.clients[&client].mode, Mode::AccountForm(f) if f.name == "another account draft"));

        server.clients.get_mut(&client).unwrap().mode = Mode::Normal;
        server.clients.get_mut(&client).unwrap().adding_account = Some(login);
        server.workspaces[0].tab_mut().focus = Some(login + 1);
        server.added_login_finished(login, &a);
        assert!(matches!(server.clients[&client].mode, Mode::Normal));
        assert_eq!(server.clients[&client].adding_account, None);

        server.workspaces[0].tab_mut().focus = Some(login);
        server.clients.get_mut(&client).unwrap().adding_account = Some(login);
        server.added_login_finished(login + 1, &a);
        assert!(matches!(server.clients[&client].mode, Mode::Normal));
        assert_eq!(server.clients[&client].adding_account, Some(login));
        server.added_login_finished(login, &a);
        let Mode::Picker(p) = &server.clients[&client].mode else { panic!("did not return to Accounts") };
        assert!(p.kind == PickerKind::Accounts);
        assert_eq!(p.message.as_deref(), Some("Account added"));
        assert!(matches!(&p.current().unwrap().action, PickAction::AccountDetails(id) if id == &a.id));
        assert_eq!(server.clients[&client].adding_account, None);
    }

    #[test]
    fn cancelling_or_closing_an_added_login_clears_its_return_to_accounts() {
        use crate::pane::{Login, LoginState, PaneSpec};

        let mut server = server();
        let (client, _peer) = super::super::tests::client(&mut server);
        let a = server.accounts.add(account()).unwrap();
        for cancel in [true, false] {
            let batch = crate::pane::batch();
            let spec = PaneSpec {
                command: vec!["true".into()],
                login: Some(Login { account: a.clone(), state: LoginState::Running, lock: None }),
                ..Default::default()
            };
            let login = server.spawn_pane(None, spec).unwrap();
            drop(batch);
            server.workspaces[0].tab_mut().focus = Some(login);
            server.clients.get_mut(&client).unwrap().adding_account = Some(login);
            if cancel {
                server.client_input(client, b"\x03");
                assert_eq!(server.panes[&login].spec.login.as_ref().unwrap().state, LoginState::Cancelled);
            } else {
                server.close_pane(login);
            }
            assert_eq!(server.clients[&client].adding_account, None);
            server.added_login_finished(login, &a);
            assert!(matches!(server.clients[&client].mode, Mode::Normal));
        }
    }

    #[test]
    fn an_existing_form_rejects_concurrent_review_and_rename_without_changing_the_file() {
        let dir = std::env::temp_dir().join(format!("kraal-stale-account-form-{}", accounts::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        for reviewed in [false, true] {
            let file = dir.join(format!("{reviewed}.toml"));
            let existing = Accounts::update_at(&file, |all| {
                all.add(account())?;
                let existing = all.unlisted().into_iter().find(|a| a.agent == "claude").unwrap();
                if reviewed {
                    all.adopt_checked(&existing, None)?;
                    Ok(all.reviews().pop().unwrap())
                } else {
                    Ok(existing)
                }
            })
            .unwrap();
            // The form keeps its snapshot while another client adopts, reviews and renames.
            let renamed = Accounts::update_at(&file, |all| {
                all.adopt_checked(&existing, None)?;
                let pending = all.reviews().pop().unwrap();
                let a = all.review(&pending, None)?;
                all.rename(&a.agent, &a.key(), "other-client")?;
                Ok(a.id)
            })
            .unwrap();
            let before = std::fs::read_to_string(&file).unwrap();
            let checked = account_status::Checked { status: Status::SignedIn, identity: None };
            let result =
                Accounts::update_at(&file, |all| name_existing_account(all, &existing, &checked, "stale-name"));
            assert!(result.unwrap_err().to_string().contains("existing account changed"));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
            assert_eq!(Accounts::load_from(&file).unwrap().by_id(&renamed).unwrap().name, "other-client");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_existing_form_names_only_its_unchanged_pending_account() {
        for reviewed in [false, true] {
            let mut all = Accounts::empty();
            let mut existing = all.unlisted().into_iter().find(|a| a.agent == "claude").unwrap();
            if reviewed {
                all.add(account()).unwrap();
                all.adopt_checked(&existing, None).unwrap();
                existing = all.reviews().pop().unwrap();
            }
            let mut stale = existing.clone();
            let checked = account_status::Checked { status: Status::SignedIn, identity: None };
            stale.id = accounts::new_uuid();
            assert!(name_existing_account(&mut all, &stale, &checked, "wrong-id").is_err());
            stale = existing.clone();
            stale.home = "/nonexistent/replaced".into();
            assert!(name_existing_account(&mut all, &stale, &checked, "wrong-home").is_err());
            let named = name_existing_account(&mut all, &existing, &checked, "personal").unwrap();
            assert_eq!(named.name, "personal");
            assert!(named.same_home(&existing));
            assert!(!named.review);
            assert!(!named.sign_in_required);
            assert_eq!(all.for_agent("claude").len(), if reviewed { 2 } else { 1 });
        }
    }

    #[test]
    fn a_pending_review_can_be_named_after_sign_out_but_uncertain_checks_change_nothing() {
        let dir = std::env::temp_dir().join(format!("kraal-signed-out-review-{}", accounts::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        for review in [false, true] {
            for status in [Status::SignedIn, Status::NotSignedIn, Status::NotInstalled, Status::CouldNotCheck] {
                let file = dir.join(format!("{review}-{status:?}.toml"));
                let existing = Accounts::update_at(&file, |all| {
                    all.add(account())?;
                    let existing = all.unlisted().into_iter().find(|a| a.agent == "claude").unwrap();
                    if review {
                        all.adopt_checked(&existing, None)?;
                        Ok(all.reviews().pop().unwrap())
                    } else {
                        Ok(existing)
                    }
                })
                .unwrap();
                let before = std::fs::read_to_string(&file).unwrap();
                let checked = account_status::Checked { status, identity: None };
                let result =
                    Accounts::update_at(&file, |all| name_existing_account(all, &existing, &checked, "personal"));
                if status == Status::SignedIn || review && status == Status::NotSignedIn {
                    let named = result.unwrap();
                    assert_eq!(named.id, existing.id);
                    assert_eq!(named.name, "personal");
                    assert!(named.same_home(&existing));
                    assert!(!named.review);
                    assert_eq!(named.sign_in_required, status == Status::NotSignedIn);
                    let saved = Accounts::load_from(&file).unwrap();
                    assert_eq!(saved.by_id(&named.id).unwrap(), named);
                    assert!(saved.reviews().is_empty());
                    assert_eq!(saved.for_agent("claude").len(), 2);
                } else {
                    assert!(result.unwrap_err().to_string().contains(status.label()));
                    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn account_summaries_distinguish_missing_expired_and_required_usage() {
        let mut a = account();
        let mut u = Usage {
            five_hour: Some(Window { used_percent: 57.0, resets_at: Some(2000) }),
            observed_at: 1000,
            ..Default::default()
        };
        assert_eq!(usage_summary(&a, None, 1100), "Usage unavailable");
        assert_eq!(usage_summary(&a, Some(&Usage::default()), 1100), "Usage unavailable");
        assert_eq!(usage_summary(&a, Some(&u), 1100), "57% used");
        assert_eq!(usage_summary(&a, Some(&u), 2000), "Last checked 16m ago");
        u.weekly = Some(Window { used_percent: 71.0, resets_at: None });
        assert_eq!(usage_summary(&a, Some(&u), 2000), "71% used");
        a.sign_in_required = true;
        assert_eq!(usage_summary(&a, Some(&u), 1100), "Sign-in required");
    }

    #[test]
    fn sidebar_summaries_fit_without_cutting_words_or_showing_zero_panes() {
        for (summary, width, expected) in [
            ("Usage unavailable", 25, "Usage unavailable"),
            ("Usage unavailable · 1 pane", 25, "No usage · 1 pane"),
            ("Usage unavailable · 2 panes", 18, "No usage · 2 panes"),
            ("57% used · 123 panes", 18, "57% · 123 panes"),
            ("Last checked 16m ago", 18, "Checked 16m ago"),
            ("Sign-in required", 12, "Sign in"),
            ("Could not verify · Check again", 18, "Retry check"),
        ] {
            assert_eq!(sidebar_summary(summary, width), expected);
            assert!(crate::text::columns(expected) <= width);
        }
        let mut server = server();
        let a = server.accounts.add(account()).unwrap();
        assert_eq!(server.account_summary_at(&a, 1100), "Usage unavailable");
        server.usage.0.insert(
            a.key(),
            Usage {
                five_hour: Some(Window { used_percent: 57.0, resets_at: Some(2000) }),
                observed_at: 1000,
                ..Default::default()
            },
        );
        assert_eq!(server.account_summary_at(&a, 1100), "57% used");
    }

    #[test]
    fn pending_sign_in_requires_a_choice_before_a_name_and_enter_only_opens_review() {
        let mut server = server();
        server.accounts.add(account()).unwrap();
        let existing = server.accounts.get("claude", "default").unwrap();
        server.accounts.adopt_checked(&existing, Some("me@example.com")).unwrap();
        let pending = server.accounts.reviews().pop().unwrap();
        let before = server.accounts.all();
        for screen in [Rect::new(0, 0, 80, 24), Rect::new(0, 0, 124, 36)] {
            let mut form = server.account_form(0);
            form.installed = true;
            assert!(form.choosing());
            assert_eq!(form.field, 2);
            assert!(form.name.is_empty());
            let mut buf = Buffer::empty(screen);
            assert!(draw_form(&form, screen, &mut buf).is_none());
            let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
            assert!(text.contains("Existing Claude sign-in needs review."));
            assert!(!text.contains("Name    "));
            assert!(text.contains("[> Use existing account]"));
            assert!(text.contains("[  Sign in to another account]"));
            assert!(text.contains("[  Cancel]"));
            let l = form_layout(&form, screen);
            assert_eq!(l.primary.y, l.name.y + 1);
            assert_eq!(l.alternate.unwrap().y, l.primary.y + 1);
            assert_eq!(l.cancel.y, l.primary.y + 2);
            let Mode::Picker(p) = server.on_account_form(0, form.clone(), key("enter")) else { panic!() };
            assert!(p.kind == PickerKind::AccountReview);
            let Mode::AccountForm(form) = server.on_account_form(0, form, key("right")) else { panic!() };
            assert_eq!(form.field, 3);
            let mut buf = Buffer::empty(screen);
            draw_form(&form, screen, &mut buf);
            assert!(
                buf.content.iter().map(|c| c.symbol()).collect::<String>().contains("[> Sign in to another account]")
            );
            let Mode::AccountForm(form) = server.on_account_form(0, form, key("enter")) else { panic!() };
            assert!(!form.choosing());
            assert!(matches!(form.choice, Some(AccountChoice::New)));
            assert_eq!(form.field, 1);
            assert!(form.name.is_empty());
            let mut buf = Buffer::empty(screen);
            assert!(draw_form(&form, screen, &mut buf).is_some());
            let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
            assert!(text.contains("Name    e.g. Work or Personal"));
            assert!(text.contains("[> Sign in]  [  Cancel]"));
            let Mode::AccountForm(form) = server.on_account_form(0, form, key("enter")) else { panic!() };
            assert!(form.error.is_some());
        }
        assert_eq!(server.accounts.all(), before);
        assert_eq!(server.accounts.reviews(), std::slice::from_ref(&pending));
        let form = server.name_existing_form(0, pending);
        assert_eq!(form.name, "me@example.com");
        assert!(matches!(form.choice, Some(AccountChoice::Existing)));
    }

    #[test]
    fn one_click_selects_either_sign_in_path_and_background_refresh_keeps_the_choice() {
        let mut server = server();
        server.accounts.add(account()).unwrap();
        let existing = server.accounts.get("claude", "default").unwrap();
        server.accounts.adopt_checked(&existing, None).unwrap();
        let form = server.account_form(0);
        let l = form_layout(&form, server.client_screen(0));
        let click = |r: Rect| {
            InputEvent::Mouse(MouseEvent {
                kind: MouseKind::Press(MouseButton::Left),
                col: r.x,
                row: r.y,
                mods: Default::default(),
            })
        };
        assert!(
            matches!(server.on_account_form(0, form.clone(), click(l.primary)), Mode::Picker(p) if p.kind == PickerKind::AccountReview)
        );
        let Mode::AccountForm(mut form) = server.on_account_form(0, form, click(l.alternate.unwrap())) else {
            panic!()
        };
        form.name = "Work".into();
        server.fill_account_form(&mut form);
        assert_eq!(form.name, "Work");
        assert!(!form.choosing());
        assert!(matches!(form.choice, Some(AccountChoice::New)));
        let existing = server.accounts.reviews().pop().unwrap();
        let mut form = server.name_existing_form(0, existing.clone());
        server.accounts.review(&existing, None).unwrap();
        server.fill_account_form(&mut form);
        assert_eq!(form.existing, Some(existing), "a stale form keeps its snapshot for validation");
    }

    #[test]
    fn accounts_list_always_has_one_active_add_row_and_selects_by_identity() {
        let mut server = server();
        let a = server.accounts.add(account()).unwrap();
        let mut p = server.accounts_picker(Some(&a.id));
        assert_eq!(p.current().unwrap().label, "Claude · work");
        assert_eq!(p.current().unwrap().detail, "Usage unavailable");
        assert_eq!(p.items.len(), 2);
        p.query = "nothing matches".into();
        p.refilter();
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.current().unwrap().label, "+ Add account");
        assert!(!p.current().unwrap().dim);
        assert_eq!(p.action_keys(), &[("enter", "Add account")]);
        let existing = server.accounts.unlisted().into_iter().find(|a| a.agent == "claude").unwrap();
        server.accounts.adopt_checked(&existing, None).unwrap();
        assert_eq!(
            server
                .accounts_picker(None)
                .items
                .iter()
                .filter(|i| matches!(i.action, PickAction::ReviewAccount(_)))
                .count(),
            1
        );
    }

    #[test]
    fn add_form_defaults_and_missing_program_keep_values_without_creating_an_account() {
        let mut server = server();
        let form = server.account_form(0);
        assert!(form.name.is_empty());
        assert_eq!(form.agent, 0);
        let name = form.name.clone();
        let Mode::AccountForm(form) = server.on_account_form(0, form, key("enter")) else { panic!("form closed") };
        assert_eq!(form.name, name);
        assert_eq!(form.agent, 0);
        assert!(form.error.as_deref().unwrap().contains("Install Claude"));
        assert!(server.accounts.all().is_empty());
        let Mode::AccountForm(form) = server.on_account_form(0, form, key("shift+tab")) else { panic!() };
        let Mode::AccountForm(form) = server.on_account_form(0, form, key("right")) else { panic!() };
        assert_eq!(form.agent, 1);
        assert_eq!(form.name, name);
        assert!(
            matches!(server.on_account_form(0, form, key("esc")), Mode::Picker(p) if p.kind == PickerKind::Accounts)
        );
    }

    #[test]
    fn form_errors_wrap_and_scroll_in_full_at_both_sizes_and_buttons_click_once() {
        let mut server = server();
        for screen in [Rect::new(0, 0, 80, 24), Rect::new(0, 0, 124, 36)] {
            let mut form = server.account_form(0);
            form.error = Some((0..50).map(|n| format!("error line {n}\n")).collect());
            let l = form_layout(&form, screen);
            assert_eq!(l.lines.len(), 50);
            assert!(l.max_scroll > 0);
            let mut seen = String::new();
            for scroll in 0..=l.max_scroll {
                form.scroll = scroll;
                let mut buf = Buffer::empty(screen);
                draw_form(&form, screen, &mut buf);
                seen.push_str(&buf.content.iter().map(|c| c.symbol()).collect::<String>());
            }
            for n in 0..50 {
                assert!(seen.contains(&format!("error line {n}")), "line {n} missing");
            }
            assert!(l.area.contains(l.cancel.as_position()));
        }
        let form = server.account_form(0);
        let l = form_layout(&form, server.client_screen(0));
        let click = |r: Rect| {
            InputEvent::Mouse(MouseEvent {
                kind: MouseKind::Press(MouseButton::Left),
                col: r.x,
                row: r.y,
                mods: Default::default(),
            })
        };
        let Mode::AccountForm(form) = server.on_account_form(0, form, click(l.primary)) else { panic!() };
        assert!(form.error.is_some(), "one click submitted the form");
        assert!(matches!(server.on_account_form(0, form, click(l.cancel)), Mode::Picker(_)));
    }

    #[test]
    fn existing_sign_in_is_the_first_form_action_and_details_keep_unknown_usage() {
        let mut server = server();
        let a = server.accounts.add(account()).unwrap();
        let mut form = server.account_form(0);
        form.existing = Some(a.clone());
        form.installed = true;
        let screen = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(screen);
        draw_form(&form, screen, &mut buf);
        let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(text.find("Use existing account").unwrap() < text.find("Sign in to another account").unwrap());
        let Mode::AccountDetails(details) = server.account_details(&a.id) else { panic!() };
        assert_eq!(details.text, "Usage unavailable");
        assert!(matches!(server.on_account_details(0, details, key("tab")), Mode::Picker(p) if p.actions == Some(0)));
    }
    #[test]
    fn checking_an_existing_sign_in_accepts_cancel_but_never_a_second_submission() {
        let mut server = server();
        let mut form = server.account_form(0);
        form.checking = Some(42);
        let Mode::AccountForm(form) = server.on_account_form(0, form, key("enter")) else { panic!() };
        assert_eq!(form.checking, Some(42));
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
        draw_form(&form, buf.area, &mut buf);
        let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(text.contains("Checking sign-in..."));
        assert!(!text.contains("Enter Sign in"));
        assert!(matches!(server.on_account_form(0, form, key("esc")), Mode::Picker(_)));
    }
}
