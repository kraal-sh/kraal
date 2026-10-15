//! Account management shared by the list, sidebar and add form.

use std::collections::HashMap;
use std::rc::Rc;

use anyhow::{Context, Result};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::ui::{Mode, PickAction, PickItem, Picker, PickerKind, PickerState};
use super::{ClientId, Server, forms, overlays};
use crate::account_status::{self, Status};
use crate::accounts::{self, Account, Accounts, Usage};
use crate::ansi::Cursor;
use crate::input::{InputEvent, KeyCode, MouseButton, MouseKind};
use crate::layout::PaneId;
use crate::text::wrap;
use crate::theme::NoticeKind;

/// Account labels and summaries do not depend on terminal output or the general chrome tick.
pub(super) struct AccountDisplay {
    accounts: Vec<Account>,
    panes: HashMap<PaneId, PaneAccountInput>,
    usage: HashMap<String, Usage>,
    checks: HashMap<String, super::agents::ExistingCheck>,
    since: i64,
    pub until: Option<i64>,
    pub items: Vec<PickItem>,
    pub labels: HashMap<PaneId, String>,
    pub sidebar: std::cell::RefCell<Option<super::render::AccountRows>>,
}

#[derive(PartialEq)]
struct PaneAccountInput {
    reference: Option<String>,
    id: Option<String>,
    env: Vec<(String, String)>,
    observed: Option<Account>,
    state: Option<crate::agent::AgentState>,
}

impl PaneAccountInput {
    fn matches(&self, pane: &crate::pane::Pane, observed: Option<&Account>) -> bool {
        self.reference == pane.spec.account
            && self.id == pane.spec.account_id
            && self.env == pane.spec.env
            && self.observed.as_ref() == observed
            && self.state == pane.agent.kind.as_ref().map(|_| pane.agent.state)
    }
}

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
    pub back: Option<PickerState>,
}

impl AccountForm {
    fn choosing(&self) -> bool {
        self.existing.is_some() && self.choice.is_none()
    }

    fn state(&mut self) -> forms::FormState<'_> {
        forms::FormState {
            field: &mut self.field,
            scroll: &mut self.scroll,
            error: &mut self.error,
            pending: &mut self.checking,
        }
    }
}

pub struct AccountDetails {
    pub account: String,
    pub back: Option<PickerState>,
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
    let Some(u) = usage.filter(|u| u.five_hour.is_some() || u.weekly.is_some()) else {
        return "No usage yet".into();
    };
    if let Some(peak) = u.valid_peak(now) {
        format!("{peak:.0}% used")
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
    pub(super) fn account_display_at(&self, now: i64) -> Rc<AccountDisplay> {
        self.account_display(Some(now))
    }

    pub(super) fn pane_account_label(&self, pane: PaneId) -> Option<String> {
        // Frame labels do not age; composing a frame uses one moment for its usage rows.
        self.account_display(None).labels.get(&pane).cloned()
    }

    fn account_display(&self, now: Option<i64>) -> Rc<AccountDisplay> {
        let mut cached = self.account_display.borrow_mut();
        if let Some(display) = cached.as_ref()
            && display.accounts == self.accounts.entries()
            && display.usage == self.usage.0
            && display.checks == *self.account_discovery.checks()
            && now.is_none_or(|now| now >= display.since && display.until.is_none_or(|until| now < until))
            && display.panes.len() == self.panes.len()
            && self.panes.iter().all(|(id, pane)| {
                display
                    .panes
                    .get(id)
                    .is_some_and(|input| input.matches(pane, self.account_discovery.observed(id).map(|a| &a.account)))
            })
        {
            return display.clone();
        }
        let now = now.unwrap_or_else(accounts::now);
        let panes = self
            .panes
            .iter()
            .map(|(&id, pane)| {
                (
                    id,
                    PaneAccountInput {
                        reference: pane.spec.account.clone(),
                        id: pane.spec.account_id.clone(),
                        env: pane.spec.env.clone(),
                        observed: self.account_discovery.observed(&id).map(|a| a.account.clone()),
                        state: pane.agent.kind.as_ref().map(|_| pane.agent.state),
                    },
                )
            })
            .collect();
        let labels =
            self.panes.keys().filter_map(|&id| self.pane_account(id).map(|a| (id, accounts::label(&a)))).collect();
        let display = Rc::new(AccountDisplay {
            accounts: self.accounts.entries().to_vec(),
            panes,
            usage: self.usage.0.clone(),
            checks: self.account_discovery.checks().clone(),
            since: now,
            until: self.account_summaries_until(now),
            items: self.account_items_at(now),
            labels,
            sidebar: Default::default(),
        });
        *cached = Some(display.clone());
        display
    }

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
        let mut p = Picker::new(PickerKind::Accounts, "Accounts", self.account_display_at(now).items.clone());
        if let Some(id) = selected {
            p.select(|i| matches!(&i.action, PickAction::AccountDetails(a) if a == id));
        }
        p
    }

    fn account_items_at(&self, now: i64) -> Vec<PickItem> {
        #[cfg(test)]
        ACCOUNT_BUILDS.with(|n| n.set(n.get() + 1));
        let mut items: Vec<_> = self
            .accounts
            .all()
            .iter()
            .map(|a| PickItem::new(label(a), self.account_summary_at(a, now), PickAction::AccountDetails(a.id.clone())))
            .collect();
        for a in self.accounts.unlisted() {
            use super::agents::ExistingCheck;
            let detail = match self.account_discovery.checks().get(&a.agent) {
                Some(ExistingCheck::Checking) => "Checking sign-in",
                Some(ExistingCheck::SaveFailed) => "Could not save · Check again",
                Some(ExistingCheck::Checked(Status::CouldNotCheck)) => "Could not verify · Check again",
                _ => continue,
            };
            let item = if self.accounts.for_agent(&a.agent).is_empty() {
                PickItem::new(accounts::existing_label(&a.agent), detail, PickAction::CheckExisting { agent: a.agent })
            } else {
                let home = crate::paths::shorten_home(&crate::paths::expand_tilde(&a.home));
                let (label, detail) =
                    if matches!(self.account_discovery.checks().get(&a.agent), Some(ExistingCheck::Checking)) {
                        (format!("Checking {home}"), "")
                    } else {
                        (format!("Could not check {home}"), detail)
                    };
                PickItem::new(label, detail, PickAction::CheckExisting { agent: a.agent }).dim()
            };
            items.push(item);
        }
        for a in self.accounts.reviews() {
            let item = if matches!(
                self.account_discovery.checks().get(&a.agent),
                Some(super::agents::ExistingCheck::SaveFailed)
            ) {
                PickItem::new(
                    accounts::review_question(&a),
                    "Could not save · Check again",
                    PickAction::CheckExisting { agent: a.agent },
                )
            } else {
                PickItem::new(accounts::review_question(&a), "", PickAction::ReviewAccount(a))
            };
            items.push(item.dim());
        }
        items.push(PickItem::new("+ Add account", "", PickAction::AddAccount));
        items
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

    pub(super) fn account_details(&self, id: &str, from: Option<&Picker>) -> Mode {
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
        Mode::AccountDetails(AccountDetails {
            account: a.id.clone(),
            back: from.and_then(Picker::form_back),
            title: label(&a),
            text,
            scroll: 0,
        })
    }

    pub(super) fn account_form(&self, client: ClientId) -> AccountForm {
        let agent = self
            .client_focus(client)
            .and_then(|p| {
                self.pane_account(p).map(|a| a.agent).or_else(|| self.panes.get(&p).and_then(|p| p.agent.kind.clone()))
            })
            .filter(|a| accounts::agent_kind(a).is_some())
            .or_else(|| self.clients.get(&client).and_then(|c| c.last_account_agent.clone()));
        let agent = accounts::agents().position(|k| Some(k.name) == agent.as_deref()).unwrap_or(0);
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
            back: None,
        };
        self.fill_account_form(&mut form);
        form
    }

    pub(super) fn fill_account_form(&self, form: &mut AccountForm) {
        let choosing = form.choosing();
        let kind = accounts::agents().nth(form.agent).unwrap();
        form.installed = crate::transfer::have_program(kind.command);
        if matches!(form.choice, Some(AccountChoice::Existing)) {
            return;
        }
        form.existing = self.accounts.reviews().into_iter().chain(self.accounts.unlisted()).find(|a| {
            a.agent == kind.name
                && (a.review
                    || matches!(
                        self.account_discovery.checks().get(&a.agent),
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
        form.agent = accounts::agents().position(|k| k.name == existing.agent).unwrap_or(0);
        form.installed = crate::transfer::have_program(accounts::agents().nth(form.agent).unwrap().command);
        form.name = self.accounts.proposed_name(&existing.agent, existing.public_identity.as_deref());
        form.cursor = form.name.chars().count();
        form.existing = Some(existing);
        form.choice = Some(AccountChoice::Existing);
        form.field = 1;
        form
    }

    fn account_added(&mut self, client: ClientId, account: &Account, back: Option<PickerState>) {
        let mut picker = self.accounts_picker(Some(&account.id));
        picker.close_to = back.map(Box::new);
        let result = picker.result(NoticeKind::Notice, "Account added");
        if let Some(c) = self.clients.get_mut(&client) {
            c.mode = result;
        }
        self.mark_dirty();
    }

    fn use_existing_form(&mut self, client: ClientId, mut form: AccountForm) -> Mode {
        let existing = form.existing.clone().unwrap();
        let checked = existing.clone();
        let name = form.name.trim().to_string();
        let token = self.alloc_id();
        if !form.state().begin(token) {
            return Mode::AccountForm(form);
        }
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
                let result = server.update_accounts(|all| name_existing_account(all, &existing, &status, &name));
                server.reload_accounts();
                let mut form = saved_form;
                form.state().complete(token, result.as_ref().err().map(|e| format!("{e:#}")));
                match result {
                    Ok(a) => server.account_added(client, &a, form.back),
                    Err(_) => {
                        server.clients.get_mut(&client).unwrap().mode = Mode::AccountForm(form);
                        server.mark_dirty();
                    }
                }
            },
        ) {
            form.state().complete(token, Some("Could not start the sign-in check. Try again.".into()));
        }
        Mode::AccountForm(form)
    }

    pub(super) fn on_account_form(&mut self, client: ClientId, mut form: AccountForm, event: InputEvent) -> Mode {
        let layout = form_layout(&form, self.client_screen(client));
        let buttons = form_buttons(&form);
        let mut fields = vec![forms::Field { id: 0, area: layout.agent, value: forms::FieldValue::Choice }];
        if !form.choosing() {
            fields.push(forms::Field {
                id: 1,
                area: layout.name,
                value: forms::FieldValue::Text { value: &mut form.name, cursor: &mut form.cursor },
            });
        }
        let outcome = forms::FormState {
            field: &mut form.field,
            scroll: &mut form.scroll,
            error: &mut form.error,
            pending: &mut form.checking,
        }
        .handle(event, &mut fields, &buttons, &layout);
        if form.checking.is_some() {
            return if matches!(outcome, forms::FormEvent::Cancel) {
                self.form_back(client, form.back)
            } else {
                Mode::AccountForm(form)
            };
        }
        if let forms::FormEvent::Choice { field: 0, keyboard } = outcome {
            form.agent = (form.agent + 1) % accounts::agents().count();
            form.choice = None;
            self.fill_account_form(&mut form);
            if keyboard {
                form.error = None;
                form.scroll = 0;
            }
        }
        if let Some(c) = self.clients.get_mut(&client) {
            c.last_account_agent = Some(accounts::agents().nth(form.agent).unwrap().name.into());
        }
        if matches!(outcome, forms::FormEvent::Cancel) {
            return self.form_back(client, form.back);
        }
        if matches!(outcome, forms::FormEvent::Submit) {
            if form.choosing() {
                if form.field == 3 {
                    form.choice = Some(AccountChoice::New);
                    form.field = 1;
                    form.error = None;
                    return Mode::AccountForm(form);
                }
                let existing = form.existing.as_ref().unwrap();
                if existing.review {
                    let mut picker = self.review_account_picker(existing);
                    picker.close_to = form.back.map(Box::new);
                    return Mode::Picker(picker);
                }
                let mut next = self.name_existing_form(client, existing.clone());
                next.back = form.back;
                return Mode::AccountForm(next);
            }
            form.installed = crate::transfer::have_program(accounts::agents().nth(form.agent).unwrap().command);
            form.scroll = 0;
            if !form.installed {
                form.error = Some(install_message(form.agent));
            } else if form.name.trim().is_empty() {
                form.error = Some("Enter a name for this account.".into());
                form.field = 1;
            } else if matches!(form.choice, Some(AccountChoice::Existing)) {
                return self.use_existing_form(client, form);
            } else {
                match self.add_account(client, accounts::agents().nth(form.agent).unwrap().name, form.name.trim()) {
                    Ok(pane) => {
                        if let Some(c) = self.clients.get_mut(&client) {
                            c.adding_account = Some(pane);
                            c.adding_account_back = form.back;
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
        if let InputEvent::Mouse(m) = &event
            && !area.contains((m.col, m.row).into())
            && let Some(mode) = self.sidebar_account_actions(client, *m)
        {
            return mode;
        }
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
            let mut picker = self.accounts_picker(None);
            if let Some(state) = details.back {
                picker.restore(state);
            } else {
                picker.standalone_actions = true;
            }
            select_account(&mut picker, &details.account);
            if matches!(picker.current().map(|i| &i.action), Some(PickAction::AccountDetails(_))) {
                picker.actions = Some(0);
            }
            Mode::Picker(picker)
        } else if back {
            self.form_back(client, details.back)
        } else {
            Mode::AccountDetails(details)
        }
    }

    pub(super) fn added_login_finished(&mut self, pane: PaneId, account: &Account) {
        let clients: Vec<_> = self
            .clients
            .iter()
            .filter(|&(id, c)| {
                c.adding_account == Some(pane) && matches!(c.mode, Mode::Normal) && self.client_focus(*id) == Some(pane)
            })
            .map(|(&id, c)| (id, c.adding_account_back.clone()))
            .collect();
        self.abandon_added_login(pane);
        for (id, back) in clients {
            self.account_added(id, account, back);
        }
    }

    pub(super) fn abandon_added_login(&mut self, pane: PaneId) {
        for c in self.clients.values_mut() {
            if c.adding_account == Some(pane) {
                c.adding_account = None;
                c.adding_account_back = None;
            }
        }
    }
}

pub(super) fn select_account(picker: &mut Picker, account: &str) {
    let matches = |i: &PickItem| matches!(&i.action, PickAction::AccountDetails(id) if id == account);
    if !picker.filtered.iter().any(|&i| matches(&picker.items[i])) {
        picker.query.clear();
        picker.cursor = 0;
        picker.refilter();
    }
    picker.select(matches);
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
        all.adopt_checked(existing, checked.identity.as_ref().map(|i| i.email.as_str()))?;
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
    let kind = accounts::agents().nth(agent).unwrap();
    format!(
        "{} is not installed. Install {} and make `{}` available on PATH, then try again.",
        agent_name(kind.name),
        agent_name(kind.name),
        kind.command
    )
}

struct FormLayout {
    agent: Rect,
    name: Rect,
    form: forms::FormLayout,
}

impl std::ops::Deref for FormLayout {
    type Target = forms::FormLayout;

    fn deref(&self) -> &Self::Target {
        &self.form
    }
}

fn form_buttons(form: &AccountForm) -> forms::Buttons<'static> {
    forms::Buttons {
        primary: forms::Button {
            field: 2,
            label: if form.choosing() || matches!(form.choice, Some(AccountChoice::Existing)) {
                "Use existing account"
            } else {
                "Sign in"
            },
        },
        alternate: form.choosing().then_some(forms::Button { field: 3, label: "Sign in to another account" }),
        cancel: forms::Button { field: 4, label: "Cancel" },
    }
}

fn form_layout(form: &AccountForm, screen: Rect) -> FormLayout {
    let (message, error) = if form.checking.is_some() {
        ("Checking sign-in...".into(), None)
    } else if let Some(error) = form.error.as_deref() {
        (String::new(), Some(error))
    } else {
        (if form.installed { String::new() } else { install_message(form.agent) }, None)
    };
    let layout = forms::FormLayout::new(screen, 2, &message, error, &form_buttons(form));
    FormLayout { agent: layout.fields[0], name: layout.fields[1], form: layout }
}

pub(super) fn draw_form(form: &AccountForm, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let l = form_layout(form, screen);
    if l.area.width < 20 || l.area.height < 7 {
        return None;
    }
    let theme = crate::theme::current();
    overlays::frame(l.area, "Add account", theme.accent, buf);
    let mut agent = format!("Agent   {}  (Left/Right)", agent_name(accounts::agents().nth(form.agent).unwrap().name));
    if !form.installed {
        agent.push_str(" · Not installed");
    }
    buf.set_stringn(l.agent.x, l.agent.y, agent, l.agent.width as usize, forms::field_style(form.field == 0));
    let cursor = if form.choosing() {
        let message =
            format!("Existing {} sign-in needs review.", agent_name(accounts::agents().nth(form.agent).unwrap().name));
        buf.set_stringn(l.name.x, l.name.y, message, l.name.width as usize, Style::default());
        None
    } else {
        forms::draw_text_field(
            l.name,
            "Name    ",
            &form.name,
            form.cursor,
            "e.g. Work or Personal",
            form.field == 1,
            buf,
        )
    };
    forms::draw_message(&l, form.scroll, buf);
    let buttons = form_buttons(form);
    let verb = match form.field {
        3 => "Choose",
        4 => "Cancel",
        _ if form.choosing() => "Review",
        _ => buttons.primary.label,
    };
    forms::draw_buttons(&l, &buttons, form.field, form.checking, verb, buf);
    if form.checking.is_some() { None } else { cursor }
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
            id: crate::identity::new_uuid(),
            agent: "claude".into(),
            name: "work".into(),
            home: "/nonexistent/work".into(),
            threshold: None,
            note: None,
            sign_in_required: false,
            login_revision: String::new(),
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

    fn click(area: Rect) -> InputEvent {
        InputEvent::Mouse(MouseEvent {
            kind: MouseKind::Press(MouseButton::Left),
            col: area.x,
            row: area.y,
            mods: Default::default(),
        })
    }

    #[test]
    fn account_details_and_their_rename_action_keep_the_origin() {
        let mut server = server();
        let (client, _peer) = super::super::tests::client(&mut server);
        let account = server.accounts.add(account()).unwrap();
        let mut list = server.accounts_picker(Some(&account.id));
        list.query = "work".into();
        list.cursor = 2;
        list.refilter();
        for from_list in [false, true] {
            let from = from_list.then_some(&list);
            for rename in [false, true] {
                let Mode::AccountDetails(details) = server.account_details(&account.id, from) else { panic!() };
                let mode = if rename {
                    let Mode::Picker(mut picker) = server.on_account_details(client, details, key("tab")) else {
                        panic!()
                    };
                    picker.actions =
                        Some(picker.action_keys().iter().position(|(_, label)| *label == "Rename").unwrap());
                    server.clients.get_mut(&client).unwrap().mode = Mode::Picker(picker);
                    server.client_input(client, b"\r");
                    assert!(matches!(server.clients[&client].mode, Mode::Prompt(_)));
                    server.client_input(client, b"\x1b");
                    server.flush_client_input(client);
                    std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal)
                } else {
                    server.on_account_details(client, details, key("esc"))
                };
                forms::assert_return(mode, from);
            }
        }
    }

    #[test]
    fn account_details_actions_keep_the_account_after_a_concurrent_rename() {
        for query in ["", "work", "claude"] {
            let mut server = server();
            let (client, _peer) = super::super::tests::client(&mut server);
            let mut first = account();
            first.name = "first".into();
            first.home = "/nonexistent/first".into();
            server.accounts.add(first).unwrap();
            let account = server.accounts.add(account()).unwrap();
            let mut list = server.accounts_picker(None);
            list.query = query.into();
            list.cursor = query.len();
            list.refilter();
            select_account(&mut list, &account.id);
            let Mode::AccountDetails(details) = server.account_details(&account.id, Some(&list)) else { panic!() };
            // Another client renames the account while these details are still open.
            server.accounts.rename("claude", "work", "personal").unwrap();
            let Mode::Picker(picker) = server.on_account_details(client, details, key("tab")) else { panic!() };
            assert!(
                matches!(picker.current().map(|i| &i.action), Some(PickAction::AccountDetails(id)) if *id == account.id)
            );
            assert_eq!(picker.current().unwrap().label, "Claude · personal");
            assert_eq!(picker.query, if query == "work" { "" } else { query });
            assert_eq!(picker.cursor, picker.query.len());
            assert_eq!(picker.actions, Some(0));
            assert_eq!(picker.action_keys(), &[("ctrl+r", "Rename"), ("ctrl+l", "Sign in"), ("ctrl+x", "Remove")]);
        }
    }

    #[test]
    fn renaming_a_filtered_account_keeps_the_renamed_account_selected() {
        if !crate::testing::in_own_process(
            "server::account_ui::tests::renaming_a_filtered_account_keeps_the_renamed_account_selected",
        ) {
            return;
        }
        let mut server = server();
        let (client, _peer) = super::super::tests::client(&mut server);
        let account = server.update_accounts(|all| all.add(account())).unwrap();
        server.accounts = Accounts::load().unwrap();
        let mut list = server.accounts_picker(Some(&account.id));
        list.query = "work".into();
        list.cursor = 4;
        list.refilter();
        server.clients.get_mut(&client).unwrap().mode = Mode::Picker(list);
        server.client_input(client, b"\x12");
        assert!(matches!(server.clients[&client].mode, Mode::Prompt(_)));
        server.client_input(client, b"\x15personal\r");
        let Mode::Picker(picker) = &server.clients[&client].mode else { panic!("expected the Accounts list") };
        assert_eq!(picker.query, "");
        assert_eq!(picker.cursor, 0);
        assert!(
            matches!(picker.current().map(|i| &i.action), Some(PickAction::AccountDetails(id)) if *id == account.id)
        );
        assert_eq!(picker.current().unwrap().label, "Claude · personal");
        assert_eq!(Accounts::load().unwrap().by_id(&account.id).unwrap().name, "personal");
    }

    #[test]
    fn reviewing_an_existing_account_keeps_the_add_forms_origin() {
        let mut server = server();
        let (client, _peer) = super::super::tests::client(&mut server);
        let mut existing = account();
        existing.review = true;
        server.accounts.add(account()).unwrap();
        for from_list in [false, true] {
            let mut list = server.accounts_picker(None);
            list.query = "search".into();
            list.cursor = 2;
            list.refilter();
            let from = from_list.then_some(&list);
            for step in 0..3 {
                let mut form = server.account_form(client);
                form.existing = Some(existing.clone());
                form.choice = None;
                form.field = 2;
                form.back = from.and_then(Picker::form_back);
                let Mode::Picker(picker) = server.on_account_form(client, form, key("enter")) else { panic!() };
                let mode = if step > 0 {
                    let item = picker
                        .items
                        .iter()
                        .find(|i| matches!(&i.action, PickAction::UseExisting { target, .. } if target.is_some() == (step == 2)))
                        .unwrap()
                        .clone();
                    let mode = server.pick(client, &picker, item);
                    if step == 2 {
                        assert!(matches!(mode, Mode::Confirm(_)));
                        server.clients.get_mut(&client).unwrap().mode = mode;
                        server.client_input(client, b"\r");
                        std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal)
                    } else {
                        mode
                    }
                } else {
                    Mode::Picker(picker)
                };
                server.clients.get_mut(&client).unwrap().mode = mode;
                server.client_input(client, b"\x1b");
                server.flush_client_input(client);
                let mode = std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal);
                forms::assert_return(mode, from);
            }
        }
    }

    #[test]
    fn account_cancel_and_results_preserve_the_origin_even_during_checks() {
        let mut server = server();
        let (client, _peer) = super::super::tests::client(&mut server);
        let account = server.accounts.add(account()).unwrap();
        for size in [(80, 24), (124, 36)] {
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            for origin in 0..3 {
                let mut list = server.accounts_picker(Some(&account.id));
                list.query = "work".into();
                list.cursor = 2;
                list.refilter();
                list.standalone_actions = origin == 1;
                let from = (origin != 2).then_some(&list);
                for existing in [false, true] {
                    for checking in [false, true] {
                        for leave in 0..3 {
                            if checking && leave == 2 {
                                continue;
                            }
                            let mut form = if existing {
                                server.name_existing_form(client, account.clone())
                            } else {
                                server.account_form(client)
                            };
                            form.back = from.and_then(Picker::form_back);
                            form.checking = checking.then_some(42);
                            let layout = form_layout(&form, server.client_screen(client));
                            let event = match leave {
                                0 => key("esc"),
                                1 => click(layout.cancel),
                                _ => {
                                    form.field = form_buttons(&form).cancel.field;
                                    key("enter")
                                }
                            };
                            forms::assert_return(server.on_account_form(client, form, event), from);
                        }
                    }
                }
                for login in [false, true] {
                    for leave in [b"\x1b".as_slice(), b"\x03", b"\x07"] {
                        if login {
                            let pane = server.alloc_id();
                            server.workspaces[0].tab_mut().unwrap().focus = Some(pane);
                            let c = server.clients.get_mut(&client).unwrap();
                            c.mode = Mode::Normal;
                            c.adding_account = Some(pane);
                            c.adding_account_back = from.and_then(Picker::form_back);
                            server.added_login_finished(pane, &account);
                            assert!(server.clients[&client].adding_account_back.is_none());
                        } else {
                            server.account_added(client, &account, from.and_then(Picker::form_back));
                        }
                        assert!(
                            matches!(&server.clients[&client].mode, Mode::Picker(p) if p.message.as_deref() == Some("Account added"))
                        );
                        server.client_input(client, leave);
                        server.flush_client_input(client);
                        let mode = std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal);
                        forms::assert_return(mode, from);
                    }
                }
            }
        }
    }

    #[test]
    fn cached_accounts_follow_identity_pane_and_sign_in_changes() {
        let _batch = crate::pane::batch();
        let mut server = server();
        let a = server.accounts.add(account()).unwrap();
        let display = server.account_display_at(1_000);
        assert!(Rc::ptr_eq(&display, &server.account_display_at(1_001)));
        let spec = crate::pane::PaneSpec {
            command: vec!["true".into()],
            account: Some("claude:work".into()),
            account_id: Some(a.id.clone()),
            ..Default::default()
        };
        let pane = server.spawn_pane(None, spec).unwrap();
        let display = server.account_display_at(1_001);
        assert_eq!(display.items[0].detail, "No usage yet · 1 pane");
        assert_eq!(display.labels[&pane], "claude·work");
        server.accounts.rename("claude", "work", "renamed").unwrap();
        let display = server.account_display_at(1_001);
        assert_eq!(display.items[0].label, "Claude · renamed");
        assert_eq!(display.labels[&pane], "claude·renamed");
        server.accounts.set_sign_in_required(&a, true);
        assert_eq!(server.account_display_at(1_001).items[0].detail, "Sign-in required");
        server.accounts.remove("claude", "renamed");
        let replacement = server.accounts.add(Account { id: "replacement".into(), ..a.clone() }).unwrap();
        let display = server.account_display_at(1_001);
        assert!(!display.labels.contains_key(&pane), "a reused name must not rebind the old pane");
        assert_eq!(display.items[0].detail, "No usage yet");
        server.panes.get_mut(&pane).unwrap().spec.account_id = Some(replacement.id);
        assert_eq!(server.account_display_at(1_001).items[0].detail, "No usage yet · 1 pane");
        server.panes.remove(&pane);
        assert_eq!(server.account_display_at(1_001).items[0].detail, "No usage yet");
    }

    #[test]
    fn cached_accounts_follow_usage_resets_ages_and_clock_changes() {
        let mut server = server();
        let a = server.accounts.add(account()).unwrap();
        let before = server.account_display_at(1_000);
        server.usage.0.insert(
            a.key(),
            Usage {
                five_hour: Some(Window { used_percent: 57.0, resets_at: Some(2_000) }),
                observed_at: 1_000,
                ..Default::default()
            },
        );
        let display = server.account_display_at(1_000);
        assert!(!Rc::ptr_eq(&before, &display));
        assert_eq!(display.items[0].detail, "57% used");
        assert!(Rc::ptr_eq(&display, &server.account_display_at(1_999)));
        let display = server.account_display_at(2_000);
        assert_eq!(display.items[0].detail, "Last checked 16m ago");
        assert_eq!(display.until, Some(2_020));
        assert!(Rc::ptr_eq(&display, &server.account_display_at(2_019)));
        assert_eq!(server.account_display_at(2_020).items[0].detail, "Last checked 17m ago");
        assert_eq!(server.account_display_at(1_500).items[0].detail, "57% used");
        server.usage.0.clear();
        assert_eq!(server.account_display_at(1_500).items[0].detail, "No usage yet");
    }

    #[test]
    fn cached_accounts_follow_discovery_results_and_review() {
        use super::super::agents::ExistingCheck;
        let mut server = server();
        let existing = server.accounts.get("claude", "default").unwrap();
        assert_eq!(server.account_display_at(1_000).items.len(), 1);
        for (state, detail) in [
            (ExistingCheck::Checking, "Checking sign-in"),
            (ExistingCheck::SaveFailed, "Could not save · Check again"),
            (ExistingCheck::Checked(Status::CouldNotCheck), "Could not verify · Check again"),
        ] {
            server.account_discovery.set_check("claude", state);
            let display = server.account_display_at(1_000);
            assert_eq!(display.items[0].label, "Existing Claude account");
            assert_eq!(display.items[0].detail, detail);
        }
        server.account_discovery.set_check("claude", ExistingCheck::Checked(Status::NotSignedIn));
        assert_eq!(server.account_display_at(1_000).items.len(), 1);
        let target = server.accounts.add(account()).unwrap();
        server.accounts.adopt_checked(&existing, None).unwrap();
        let display = server.account_display_at(1_000);
        assert_eq!(display.items[1].label, "? Which account is ~/.claude");
        assert!(display.items[1].detail.is_empty());
        assert!(display.items[1].dim);
        let pending = server.accounts.reviews().pop().unwrap();
        server.accounts.review(&pending, Some(&target)).unwrap();
        let display = server.account_display_at(1_000);
        assert_eq!(display.items.len(), 2);
        assert_eq!(display.items[0].label, "Claude · work");
        assert_eq!(display.items[1].label, "+ Add account");
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
        server.workspaces[0].tab_mut().unwrap().focus = Some(login);
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
        server.workspaces[0].tab_mut().unwrap().focus = Some(login + 1);
        server.added_login_finished(login, &a);
        assert!(matches!(server.clients[&client].mode, Mode::Normal));
        assert_eq!(server.clients[&client].adding_account, None);

        server.workspaces[0].tab_mut().unwrap().focus = Some(login);
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
            server.workspaces[0].tab_mut().unwrap().focus = Some(login);
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
        let dir = std::env::temp_dir().join(format!("kraal-stale-account-form-{}", crate::identity::new_uuid()));
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
            stale.id = crate::identity::new_uuid();
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
        let dir = std::env::temp_dir().join(format!("kraal-signed-out-review-{}", crate::identity::new_uuid()));
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
        assert_eq!(usage_summary(&a, None, 1100), "No usage yet");
        assert_eq!(usage_summary(&a, Some(&Usage::default()), 1100), "No usage yet");
        assert_eq!(usage_summary(&a, Some(&u), 1100), "57% used");
        assert_eq!(usage_summary(&a, Some(&u), 2000), "Last checked 16m ago");
        u.weekly = Some(Window { used_percent: 71.0, resets_at: None });
        assert_eq!(usage_summary(&a, Some(&u), 2000), "71% used");
        a.sign_in_required = true;
        assert_eq!(usage_summary(&a, Some(&u), 1100), "Sign-in required");
    }

    #[test]
    fn usage_callers_share_expiry_rules_and_keep_their_wording() {
        let a = account();
        let window = |used_percent, resets_at| Some(Window { used_percent, resets_at });
        let usage = |five_hour, weekly| Usage { five_hour, weekly, observed_at: 1000, ..Default::default() };
        let bars = |five, week, reset: &str| {
            let bar = |name, pct| format!("{name} {} {pct:.0}%", crate::theme::current().bar(pct));
            let mut parts = Vec::new();
            if let Some(pct) = five {
                parts.push(bar("5h", pct));
            }
            if let Some(pct) = week {
                parts.push(bar("7d", pct));
            }
            if !reset.is_empty() {
                parts.push(reset.into());
            }
            parts.join(" · ")
        };
        for (u, now, peak, end, reset, summary, text) in [
            (usage(None, None), 1100, None, None, None, "No usage yet", "no usage seen yet".into()),
            (usage(window(0.0, None), None), 1100, Some(0.0), Some(19000), None, "0% used", bars(Some(0.0), None, "")),
            (
                usage(window(57.0, Some(2000)), None),
                1100,
                Some(57.0),
                Some(2000),
                Some(2000),
                "57% used",
                bars(Some(57.0), None, "resets in 15m"),
            ),
            (
                usage(window(57.0, Some(2000)), None),
                2000,
                None,
                None,
                None,
                "Last checked 16m ago",
                bars(Some(0.0), None, ""),
            ),
            (
                usage(window(57.0, None), None),
                18999,
                Some(57.0),
                Some(19000),
                None,
                "57% used",
                bars(Some(57.0), None, ""),
            ),
            (
                usage(window(57.0, None), None),
                19000,
                None,
                None,
                None,
                "Last checked 5h ago",
                bars(Some(0.0), None, ""),
            ),
            (
                usage(window(95.0, None), window(71.0, None)),
                19000,
                Some(71.0),
                Some(605800),
                None,
                "71% used",
                bars(Some(0.0), Some(71.0), ""),
            ),
            (
                usage(None, window(71.0, None)),
                605800,
                None,
                None,
                None,
                "Last checked 7d ago",
                bars(None, Some(0.0), ""),
            ),
            (
                usage(window(57.0, Some(2000)), window(71.0, Some(3000))),
                1100,
                Some(71.0),
                Some(2000),
                Some(3000),
                "71% used",
                bars(Some(57.0), Some(71.0), "resets in 31m"),
            ),
            (
                usage(window(57.0, None), None),
                900,
                Some(57.0),
                Some(19000),
                None,
                "57% used",
                bars(Some(57.0), None, ""),
            ),
            (
                usage(window(57.0, Some(800)), None),
                900,
                None,
                None,
                None,
                "Last checked just now",
                bars(Some(0.0), None, ""),
            ),
            (
                Usage { observed_at: 0, ..usage(window(57.0, Some(800)), None) },
                900,
                None,
                None,
                None,
                "Usage unavailable",
                bars(Some(0.0), None, ""),
            ),
        ] {
            assert_eq!(u.valid_peak(now), peak, "{u:?} at {now}");
            assert_eq!(u.peak(now), peak.unwrap_or(0.0), "{u:?} at {now}");
            assert_eq!(u.next_end(now), end, "{u:?} at {now}");
            assert_eq!(u.next_reset(now), reset, "{u:?} at {now}");
            assert_eq!(usage_summary(&a, Some(&u), now), summary, "{u:?} at {now}");
            assert_eq!(accounts::usage_text(&u, now), text, "{u:?} at {now}");
            let map = accounts::UsageMap(HashMap::from([(a.key(), u.clone())]));
            let expected = (u.five_hour.is_some() || u.weekly.is_some()).then_some(peak.unwrap_or(0.0));
            assert_eq!(map.peak(&a, now), expected, "{u:?} at {now}");
            assert_eq!(map.next_end(now), end, "{u:?} at {now}");
        }
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
        assert_eq!(server.account_summary_at(&a, 1100), "No usage yet");
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
            let text = crate::testing::buffer_text(&buf);
            assert!(text.contains("Existing Claude sign-in needs review."));
            assert!(!text.contains("Name    "));
            assert!(text.contains(" Use existing account "));
            assert!(text.contains(" Sign in to another account "));
            assert!(text.contains(" Cancel "));
            let l = form_layout(&form, screen);
            assert_eq!(buf[l.primary.as_position()].bg, crate::theme::current().accent);
            assert_eq!(l.primary.y, l.name.y + 2);
            assert_eq!(l.alternate.unwrap().y, l.primary.y);
            assert_eq!(l.cancel.y, l.primary.y);
            let Mode::Picker(p) = server.on_account_form(0, form.clone(), key("enter")) else { panic!() };
            assert!(p.kind == PickerKind::AccountReview);
            let Mode::AccountForm(form) = server.on_account_form(0, form, key("right")) else { panic!() };
            assert_eq!(form.field, 3);
            let mut buf = Buffer::empty(screen);
            draw_form(&form, screen, &mut buf);
            assert_eq!(buf[l.alternate.unwrap().as_position()].bg, crate::theme::current().accent);
            let Mode::AccountForm(form) = server.on_account_form(0, form, key("enter")) else { panic!() };
            assert!(!form.choosing());
            assert!(matches!(form.choice, Some(AccountChoice::New)));
            assert_eq!(form.field, 1);
            assert!(form.name.is_empty());
            let mut buf = Buffer::empty(screen);
            assert!(draw_form(&form, screen, &mut buf).is_some());
            let text = crate::testing::buffer_text(&buf);
            assert!(text.contains("Name    e.g. Work or Personal"));
            assert!(text.contains(" Sign in     Cancel "));
            assert_eq!(buf[form_layout(&form, screen).primary.as_position()].bg, crate::theme::current().accent);
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
        assert_eq!(p.current().unwrap().detail, "No usage yet");
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
        assert!(matches!(server.on_account_form(0, form, key("esc")), Mode::Normal));
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
                assert_eq!(buf[l.body.as_position()].fg, crate::theme::current().blocked);
                seen.push_str(&crate::testing::buffer_text(&buf));
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
        assert!(matches!(server.on_account_form(0, form, click(l.cancel)), Mode::Normal));
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
        let text = crate::testing::buffer_text(&buf);
        assert!(text.find("Use existing account").unwrap() < text.find("Sign in to another account").unwrap());
        let Mode::AccountDetails(details) = server.account_details(&a.id, None) else { panic!() };
        assert_eq!(details.text, "No usage yet");
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
        let text = crate::testing::buffer_text(&buf);
        assert!(text.contains("Checking sign-in..."));
        assert!(!text.contains("Enter Sign in"));
        assert!(matches!(server.on_account_form(0, form, key("esc")), Mode::Normal));
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static ACCOUNT_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
