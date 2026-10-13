//! Interactive client modes: pickers, prompts, confirmations, help, and mouse drags.

use crate::agent::AgentState;
use crate::layout::{Border, PaneId};

pub enum Mode {
    Normal,
    /// The leader key was pressed; `typed` are the keys after it so far, and the menu shows
    /// what can follow. `miss` says a key led nowhere.
    Leader {
        typed: Vec<crate::input::Key>,
        miss: Option<String>,
    },
    /// Resizing panes with the arrows until Enter or Esc.
    Resize,
    /// Browsing the focused pane's scrollback.
    Scroll,
    /// Typing a search in the focused pane's scrollback.
    Search {
        query: String,
        found: bool,
    },
    Picker(Picker),
    Prompt(Prompt),
    AccountForm(super::account_ui::AccountForm),
    AccountDetails(super::account_ui::AccountDetails),
    Help {
        scroll: u16,
    },
    /// The messages kraal showed (and the warnings it had), newest first.
    Messages {
        scroll: u16,
    },
    Confirm(Question),
    /// Labels over what can be acted on in a pane.
    Hints(HintsMode),
}

/// A question with two buttons.
pub struct Question {
    /// The question on its first line, then what there is to know before answering. A line
    /// that starts with a space quotes what the question is about (a command, a list entry):
    /// the box shows it with its spacing, all of it.
    pub message: String,
    pub action: ConfirmAction,
    /// The confirming button is selected (Enter confirms); Cancel is, at first.
    pub yes: bool,
    /// First row of the message shown, when it is too long for the box.
    pub scroll: usize,
}

/// Hints in progress.
pub struct HintsMode {
    pub pane: PaneId,
    pub hints: Vec<crate::hints::Hint>,
    /// The label typed so far.
    pub typed: String,
    /// A label opens or runs its target instead of copying it.
    pub act: bool,
    /// The pane as it was when the labels were made: shown under them while the pane goes on.
    pub snapshot: Option<Box<ratatui::buffer::Buffer>>,
}

impl Mode {
    /// What the top bar says the client is doing; nothing in normal use.
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "",
            Mode::Leader { .. } => "MENU",
            Mode::Resize => "RESIZE",
            Mode::Scroll => "COPY",
            Mode::Search { .. } => "SEARCH",
            Mode::Picker(_) => "CHOOSE",
            Mode::Prompt(_) | Mode::AccountForm(_) => "INPUT",
            Mode::AccountDetails(_) => "ACCOUNT",
            Mode::Help { .. } => "HELP",
            Mode::Messages { .. } => "MESSAGES",
            Mode::Confirm(_) => "CONFIRM",
            Mode::Hints(_) => "HINTS",
        }
    }

    /// The leader menu, at its start.
    pub fn leader() -> Mode {
        Mode::Leader { typed: Vec::new(), miss: None }
    }

    /// A question, Cancel selected.
    pub fn confirm(message: impl Into<String>, action: ConfirmAction) -> Mode {
        Mode::Confirm(Question { message: message.into(), action, yes: false, scroll: 0 })
    }
}

pub enum ConfirmAction {
    RemoveAccount(crate::accounts::Account),
    ReplaceWorkspaceFile {
        prompt: Prompt,
        save: super::definitions::DefinitionSave,
    },
    UseExisting {
        existing: Box<crate::accounts::Account>,
        target: Option<crate::accounts::Account>,
    },
    ClosePane(PaneId),
    CloseTab {
        workspace: u32,
        tab: u32,
    },
    CloseWorkspace(u32),
    /// Restart the session on the kraal installed now.
    Restart,
    /// Stop the session, its layout saved.
    Quit,
    /// Run `kraal <args>` in a job pane.
    Job {
        label: String,
        args: Vec<String>,
    },
    StopAndSwitch {
        pane: PaneId,
        token: u32,
    },
    SignInAccount(crate::accounts::Account),
    RetryAccountSwitch {
        pane: PaneId,
        account: crate::accounts::Account,
    },
    /// Run a git shell script in a job pane.
    GitJob {
        label: String,
        script: String,
    },
    /// Remove exactly these worktrees, those still in the state the user was shown.
    RemoveWorktrees(Vec<(crate::git::Worktree, crate::git::WorktreeState)>),
    /// Trust this version of a repository's worktree setup, then make the worktree.
    TrustWorktreeSetup(Box<crate::server::worktrees::NewWorktree>),
    /// Start a tool on a production pane.
    RunTool {
        pane: Option<PaneId>,
        name: String,
        selection: Option<String>,
        /// The production reason the user was asked about.
        reason: crate::server::contexts::Production,
        /// Where the tool was asked for (see `Server::tool_place`): it runs there or not at all.
        place: Option<String>,
    },
    /// Edit a file of a production server in the local editor.
    Edit(Box<super::edits::EditTarget>),
    /// Trust the version of a repository runbook being prepared, then go on preparing it.
    TrustRunbook,
    /// Run the runbook being prepared (its plan is the message).
    RunRunbook,
    /// Run a block of the Markdown runbook in progress; `production` is the target's production
    /// reason the question showed (what a yes approves).
    RunBlock {
        index: usize,
        production: Option<crate::server::contexts::Production>,
    },
    /// Input about to reach production panes: approve them for synced typing and send it.
    Production {
        workspace: u32,
        /// Panes and the production reason the user is asked about.
        approve: Vec<(PaneId, crate::server::contexts::Production)>,
        pending: PendingInput,
    },
}

impl ConfirmAction {
    /// The confirming button: what will happen.
    pub fn verb(&self) -> String {
        match self {
            ConfirmAction::ReplaceWorkspaceFile { .. } => "Replace file".into(),
            ConfirmAction::UseExisting { .. } => "Use account".into(),
            ConfirmAction::RemoveAccount(_) => "Remove".into(),
            ConfirmAction::ClosePane(_) => "Close pane".into(),
            ConfirmAction::CloseTab { .. } => "Close tab".into(),
            ConfirmAction::CloseWorkspace(_) => "Close workspace".into(),
            ConfirmAction::Restart => "Restart".into(),
            ConfirmAction::Quit => "Quit".into(),
            ConfirmAction::Job { label, .. } | ConfirmAction::GitJob { label, .. } => {
                let mut l = label.clone();
                if let Some(first) = l.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                l
            }
            ConfirmAction::StopAndSwitch { .. } => "Stop and switch".into(),
            ConfirmAction::SignInAccount(_) => "Sign in".into(),
            ConfirmAction::RetryAccountSwitch { .. } => "Check again".into(),
            ConfirmAction::RemoveWorktrees(list) if list.len() == 1 => "Remove worktree".into(),
            ConfirmAction::RemoveWorktrees(_) => "Remove worktrees".into(),
            ConfirmAction::TrustWorktreeSetup(_) => "Trust this version".into(),
            ConfirmAction::RunTool { .. } => "Run tool".into(),
            ConfirmAction::Edit(_) => "Edit".into(),
            ConfirmAction::TrustRunbook => "Trust this version".into(),
            ConfirmAction::RunRunbook => "Run".into(),
            ConfirmAction::RunBlock { .. } => "Send to the pane".into(),
            ConfirmAction::Production { .. } => "Send".into(),
        }
    }

    /// Production is involved, or the action destroys something.
    pub fn danger(&self, message: &str) -> bool {
        matches!(self, ConfirmAction::Production { .. } | ConfirmAction::RemoveWorktrees(_))
            || matches!(self, ConfirmAction::Job { label, .. } if label == "delete" || label.contains("replace"))
            || message.contains("PRODUCTION")
            || message.contains(" is production (")
    }
}

/// Input held back until a production confirmation is answered: keys and pastes, in order (a
/// key, a paste, or what was typed while a context was read), and the panes they were going to
/// with what made them production then. It goes to those panes, not to wherever the focus is by
/// the time of the answer, and all of it goes or none: not at all if one of them became another
/// production meanwhile.
pub struct PendingInput {
    pub items: Vec<InputItem>,
    pub targets: Vec<(PaneId, Option<crate::server::contexts::Production>)>,
}

/// A key or a paste on its way to panes.
pub enum InputItem {
    Key(crate::input::Key),
    Paste(Vec<u8>),
    /// A piece of a paste too big to hold whole, going out as it comes; the flag says the piece
    /// before it ended with CR. Never asked about: such a paste never goes to production.
    Piece(Vec<u8>, bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// Every command, to search and run.
    Commands,
    Workspace,
    Ssh,
    Pane,
    Agent,
    AccountReview,
    Accounts,
    Tool,
    Runbook,
    RunbookTarget,
    RunbookParam,
    RunbookSteps,
    /// What to do with an edited file whose server's version changed meanwhile.
    EditConflict,
    /// The paste buffers.
    Buffers,
}

#[derive(Clone)]
pub enum PickAction {
    AccountDetails(String),
    AddAccount,
    ReviewAccount(crate::accounts::Account),
    CheckExisting {
        agent: String,
    },
    UseExisting {
        existing: crate::accounts::Account,
        target: Option<crate::accounts::Account>,
    },
    CancelSwitch {
        pane: PaneId,
        token: u32,
    },
    SwitchWorkspace(u32),
    /// A definition not running, by its file.
    OpenDefinition(std::path::PathBuf),
    NewWorkspace,
    /// A server name or `user@host[:port]`.
    Ssh(String),
    SshGroup(String),
    FocusPane(PaneId),
    /// An agent of a server's kraal session: the server, and its pane's id there.
    RemoteAgent {
        server: String,
        pane: u32,
    },
    AgentLaunch {
        agent: String,
        account: String,
    },
    AgentSwitch {
        pane: PaneId,
        agent: String,
        account: String,
        current: bool,
    },
    Worktree {
        pane: PaneId,
        op: String,
    },
    /// A runbook file to run.
    Runbook(std::path::PathBuf),
    /// Where the runbook being prepared runs: a server or `@group`.
    RunbookTarget(String),
    /// The value of the parameter being asked.
    RunbookValue(String),
    /// Run a block of the Markdown runbook in progress.
    RunBlock(usize),
    /// Back to the list of runbooks.
    RunbookList,
    /// Start a tool for a pane.
    Tool {
        pane: Option<PaneId>,
        name: String,
    },
    /// Run a command of the keymap.
    Command(crate::keymap::Command),
    /// The answer about an edited file whose server's version changed meanwhile.
    EditAnswer {
        edit: u32,
        answer: super::edits::Answer,
    },
    /// A paste buffer, by its id.
    Buffer(u64),
    /// Point a file manager panel at `~` (local) or `server:~`.
    FilesSet {
        pane: PaneId,
        side: crate::files::Side,
        target: String,
    },
    None,
}

#[derive(Clone)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
    pub action: PickAction,
    pub state: Option<AgentState>,
    /// Shown dimmed (e.g. a workspace that is defined but not running).
    pub dim: bool,
}

impl PickItem {
    pub fn new(label: impl Into<String>, detail: impl Into<String>, action: PickAction) -> PickItem {
        PickItem { label: label.into(), detail: detail.into(), action, state: None, dim: false }
    }

    pub fn dim(self) -> PickItem {
        PickItem { dim: true, ..self }
    }

    pub fn with_state(self, state: Option<AgentState>) -> PickItem {
        PickItem { state, ..self }
    }
}

pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub query: String,
    /// Cursor in `query`, in characters.
    pub cursor: usize,
    pub items: Vec<PickItem>,
    /// Indices into `items` matching the query, best first.
    pub filtered: Vec<usize>,
    pub selected: usize,
    /// The list of actions on the selected item is open, at this one.
    pub actions: Option<usize>,
    /// What the picker waits for (a list read off the event loop), by request: keys typed
    /// meanwhile stay here instead of reaching a pane.
    pub loading: Option<u32>,
    pub worktree: bool,
    pub message: Option<String>,
}

impl Picker {
    pub fn new(kind: PickerKind, title: &str, items: Vec<PickItem>) -> Picker {
        let mut p = Picker {
            kind,
            title: title.into(),
            query: String::new(),
            cursor: 0,
            items,
            filtered: Vec::new(),
            selected: 0,
            actions: None,
            loading: None,
            worktree: false,
            message: None,
        };
        p.refilter();
        p
    }

    /// Actions offered by Tab, with the keys that run them.
    pub fn action_keys(&self) -> &'static [(&'static str, &'static str)] {
        use PickAction as A;
        use PickerKind as K;
        let Some(item) = self.current() else { return &[] };
        match (self.kind, &item.action) {
            (K::Ssh, A::Ssh(_)) => &[
                ("enter", "Connect"),
                ("ctrl+t", "Connect in a new tab"),
                ("ctrl+f", "Browse its files"),
                ("ctrl+r", "Run a command on it"),
                ("ctrl+o", "Forward one of its ports"),
                ("ctrl+e", "Edit saved servers"),
                ("ctrl+x", "Remove saved server"),
            ],
            (K::Ssh, A::SshGroup(_)) => {
                &[("enter", "Open group"), ("ctrl+r", "Run a command on it"), ("ctrl+e", "Edit saved servers")]
            }
            (K::Agent, A::AgentLaunch { .. }) if self.worktree => {
                &[("ctrl+t", "Start in new tab"), ("ctrl+w", "Start in worktree"), ("ctrl+d", "Account details")]
            }
            (K::Agent, A::AgentLaunch { .. }) => &[("ctrl+t", "Start in new tab"), ("ctrl+d", "Account details")],
            (_, A::AccountDetails(_)) => &[("ctrl+r", "Rename"), ("ctrl+l", "Sign in"), ("ctrl+x", "Remove")],
            (_, A::CancelSwitch { .. }) => &[("enter", "Cancel account change")],
            (_, A::AgentSwitch { current: true, .. }) => &[("enter", "Keep current account")],
            (_, A::AgentSwitch { .. }) => &[("enter", "Switch account")],
            (_, A::ReviewAccount(_)) => &[("enter", "Review")],
            (_, A::CheckExisting { .. }) => &[("enter", "Check again")],
            (_, A::UseExisting { .. }) => &[("enter", "Choose")],
            (_, A::AddAccount) => &[("enter", "Add account")],
            (K::Workspace, A::SwitchWorkspace(_)) => &[("enter", "Open"), ("ctrl+x", "Close workspace")],
            (_, A::OpenDefinition(_)) => &[("enter", "Open workspace")],
            (_, A::NewWorkspace) => &[("enter", "Create workspace")],
            (K::Runbook, A::Runbook(_)) => &[("enter", "Run"), ("ctrl+e", "Edit")],
            (K::RunbookSteps, A::RunBlock(_)) => &[("enter", "Run step"), ("ctrl+y", "Copy step")],
            (_, A::RunbookList) => &[("enter", "Back to runbooks")],
            (_, A::Command(_)) => &[("enter", "Run")],
            (_, A::FocusPane(_) | A::RemoteAgent { .. }) => &[("enter", "Go to pane")],
            (_, A::FilesSet { .. }) => &[("enter", "Open directory")],
            (_, A::Tool { .. }) => &[("enter", "Start tool")],
            (K::Buffers, A::Buffer(_)) => &[
                ("enter", "Paste into the pane"),
                ("ctrl+y", "Copy to the clipboard again"),
                ("ctrl+o", "Show in a preview pane"),
                ("ctrl+x", "Delete"),
            ],
            (_, A::None) => &[],
            _ => &[("enter", "Choose")],
        }
    }

    pub fn refilter(&mut self) {
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| !matches!(it.action, PickAction::NewWorkspace | PickAction::AddAccount))
            .filter_map(|(i, it)| {
                // The label matters most; details only rescue items whose label does not match.
                fuzzy_score(&self.query, &it.label)
                    .or_else(|| fuzzy_score(&self.query, &it.detail).map(|s| s - 1000))
                    .map(|s| (s, i))
            })
            .collect();
        if !self.query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        if self.kind == PickerKind::Workspace
            && !self.query.trim().is_empty()
            && !self.items.iter().any(|it| it.label == self.query.trim())
            && let Some(i) = self.items.iter().position(|it| matches!(it.action, PickAction::NewWorkspace))
        {
            self.items[i].label = format!("+ new workspace \"{}\"", self.query.trim());
            self.filtered.push(i);
        }
        if self.kind == PickerKind::Ssh && !self.query.trim().is_empty() && !self.query.contains(' ') {
            let host = self.query.trim().to_string();
            if !self.items.iter().any(|it| it.label == host) {
                self.items.retain(|it| it.detail != "typed");
                self.items.push(PickItem {
                    label: host.clone(),
                    detail: "typed".into(),
                    action: PickAction::Ssh(host),
                    state: None,
                    dim: false,
                });
                self.filtered.push(self.items.len() - 1);
            }
        }
        if let Some(i) = self.items.iter().position(|it| matches!(it.action, PickAction::AddAccount)) {
            self.filtered.push(i);
        }
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    pub fn current(&self) -> Option<&PickItem> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }

    pub fn move_by(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let n = self.filtered.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(n) as usize;
    }
}

pub enum PromptAction {
    RenameTab {
        workspace: u32,
        tab: u32,
    },
    RenamePane(PaneId),
    RenameWorkspace(u32),
    /// The value is the new workspace's name.
    NewWorkspace,
    OpenPreview,
    SaveWorkspace(u32),
    /// Files dropped on an SSH pane: the value is the destination. Cancelling pastes `raw`.
    Upload {
        paths: Vec<std::path::PathBuf>,
        raw: Vec<u8>,
        pane: PaneId,
    },
    /// The value is local paths to upload to `server`.
    UploadPick {
        server: String,
    },
    /// The value is `server:path` to download into the download directory.
    Download,
    /// The value is the account's new name.
    RenameAccount {
        agent: String,
        account: String,
    },
    /// The value is the worktree's name.
    NewWorktree {
        agent: String,
        account: String,
    },
    /// The value is a commit message for the pane's worktree.
    Commit {
        pane: PaneId,
    },
    /// The value is a command to run on these servers (`kraal exec`).
    RunOn {
        targets: Vec<String>,
    },
    /// The value is `[host:]port` to forward from this server.
    Tunnel {
        server: String,
    },
    /// The value of the runbook parameter being asked.
    RunbookValue,
}

impl PromptAction {
    /// What Enter does, for the prompt's footer.
    pub fn verb(&self) -> &'static str {
        match self {
            PromptAction::RenameTab { .. }
            | PromptAction::RenamePane(_)
            | PromptAction::RenameWorkspace(_)
            | PromptAction::RenameAccount { .. } => "rename",
            PromptAction::NewWorkspace | PromptAction::NewWorktree { .. } => "create",
            PromptAction::OpenPreview => "open",
            PromptAction::SaveWorkspace(_) => "save",
            PromptAction::Upload { .. } | PromptAction::UploadPick { .. } => "upload",
            PromptAction::Download => "download",
            PromptAction::Commit { .. } => "commit",
            PromptAction::RunOn { .. } => "run",
            PromptAction::Tunnel { .. } => "forward",
            PromptAction::RunbookValue => "next",
        }
    }
}

pub struct Prompt {
    pub title: String,
    pub value: String,
    /// Cursor in `value`, in characters.
    pub cursor: usize,
    pub action: PromptAction,
    /// Why the value was not accepted; the prompt stays open to fix it.
    pub error: Option<String>,
    /// What is typed is not shown (a secret).
    pub masked: bool,
}

impl Prompt {
    pub fn new(title: &str, value: &str, action: PromptAction) -> Prompt {
        Prompt {
            title: title.into(),
            value: value.into(),
            cursor: value.chars().count(),
            action,
            error: None,
            masked: false,
        }
    }
}

/// Edit a one-line text field with `k`: cursor moves, deletions, typing. Returns false when `k`
/// is not an editing key. `cursor` counts characters.
pub fn edit_line(text: &mut String, cursor: &mut usize, k: &crate::input::Key) -> bool {
    use crate::input::KeyCode as K;
    let chars: Vec<char> = text.chars().collect();
    let mut at = (*cursor).min(chars.len());
    let word_left = |at: usize| {
        let mut i = at;
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    };
    let word_right = |at: usize| {
        let mut i = at;
        while i < chars.len() && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    };
    let (ctrl, alt) = (k.mods.ctrl, k.mods.alt);
    let mut next: Vec<char> = chars.clone();
    match k.code {
        K::Left if alt || ctrl => at = word_left(at),
        K::Right if alt || ctrl => at = word_right(at),
        K::Left => at = at.saturating_sub(1),
        K::Right => at = (at + 1).min(chars.len()),
        K::Home => at = 0,
        K::End => at = chars.len(),
        K::Backspace if alt || ctrl => {
            let from = word_left(at);
            next.drain(from..at);
            at = from;
        }
        K::Backspace if at > 0 => {
            next.remove(at - 1);
            at -= 1;
        }
        K::Backspace => {}
        K::Delete if at < chars.len() => {
            next.remove(at);
        }
        K::Delete => {}
        K::Char(c) if ctrl => match c {
            'a' => at = 0,
            'e' => at = chars.len(),
            'b' => at = at.saturating_sub(1),
            'f' => at = (at + 1).min(chars.len()),
            'd' if at < chars.len() => {
                next.remove(at);
            }
            'h' if at > 0 => {
                next.remove(at - 1);
                at -= 1;
            }
            'w' => {
                // Back to the previous space or slash, like a shell.
                let mut from = at;
                while from > 0 && matches!(chars[from - 1], ' ' | '/') {
                    from -= 1;
                }
                while from > 0 && !matches!(chars[from - 1], ' ' | '/') {
                    from -= 1;
                }
                next.drain(from..at);
                at = from;
            }
            'u' => {
                next.drain(..at);
                at = 0;
            }
            'k' => {
                next.truncate(at);
            }
            _ => return false,
        },
        K::Char('b') if alt => at = word_left(at),
        K::Char('f') if alt => at = word_right(at),
        K::Char('d') if alt => {
            let to = word_right(at);
            next.drain(at..to);
        }
        K::Char(_) if alt => return false,
        K::Char(_) if chars.len() >= MAX_FIELD => {}
        K::Char(c) => {
            next.insert(at, c);
            at += 1;
        }
        _ => return false,
    }
    *text = next.into_iter().collect();
    *cursor = at;
    true
}

/// The longest a one-line field gets: a path, a command, a name.
const MAX_FIELD: usize = 4096;

/// Insert pasted text (its first line) at the cursor; refused whole when it does not fit.
pub fn paste_line(text: &mut String, cursor: &mut usize, pasted: &str) -> Result<(), String> {
    let line = pasted.lines().next().unwrap_or("");
    if text.chars().count() + line.chars().count() > MAX_FIELD {
        return Err(format!("Too long to paste: a field holds at most {MAX_FIELD} characters."));
    }
    let at: usize = text.char_indices().nth(*cursor).map(|(i, _)| i).unwrap_or(text.len());
    text.insert_str(at, line);
    *cursor += line.chars().count();
    Ok(())
}

/// A mouse drag in progress.
pub enum Drag {
    /// Moving a split: the border, and how far before it the pointer took hold of it.
    Border(Border, u16),
    /// Selecting text: `from` is where the button went down; `moved`, whether the selection
    /// covers more than that press (a drag, or a double/triple click).
    Select {
        pane: PaneId,
        from: (u16, u16),
        moved: bool,
    },
    MoveFloat {
        pane: PaneId,
        grab_x: u16,
        grab_y: u16,
    },
    ResizeFloat {
        pane: PaneId,
    },
    /// Press went to a program that handles the mouse itself; drag and release follow it.
    Forward {
        pane: PaneId,
        button: crate::input::MouseButton,
    },
}

/// Subsequence fuzzy match. `None` if `query` does not match; higher is better.
fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut qi = 0;
    let mut prev_match: Option<usize> = None;
    for (ti, ch) in t.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            score += 1;
            if prev_match == Some(ti.wrapping_sub(1)) {
                score += 5;
            }
            if ti == 0 || matches!(t[ti - 1], ' ' | '-' | '_' | '.' | '/' | '@' | '›') {
                score += 8;
            }
            prev_match = Some(ti);
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    // Prefer shorter texts (tighter matches).
    Some(score * 4 - t.len() as i32 / 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        assert!(fuzzy_score("tp", "team-prod-nginx").unwrap() > fuzzy_score("tp", "settings-path").unwrap_or(-100));
        assert!(fuzzy_score("xyz", "tig").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        assert!(fuzzy_score("worker1", "admin@worker10.lan").is_some());
    }

    #[test]
    fn line_editing() {
        use crate::input::{Key, KeyCode, Mods};
        let key = |code: KeyCode, mods: Mods| Key { code, mods, raw: Vec::new() };
        let ch = |c: char| key(KeyCode::Char(c), Mods::NONE);
        let ctrl = |c: char| key(KeyCode::Char(c), Mods::CTRL);
        let (mut t, mut c) = (String::from("kraal web"), 9);
        assert!(edit_line(&mut t, &mut c, &key(KeyCode::Left, Mods::ALT)));
        assert_eq!(c, 6);
        edit_line(&mut t, &mut c, &ch('X'));
        assert_eq!((t.as_str(), c), ("kraal Xweb", 7));
        edit_line(&mut t, &mut c, &ctrl('a'));
        edit_line(&mut t, &mut c, &key(KeyCode::Delete, Mods::NONE));
        assert_eq!(t, "raal Xweb");
        edit_line(&mut t, &mut c, &ctrl('e'));
        edit_line(&mut t, &mut c, &ctrl('w'));
        assert_eq!(t, "raal ");
        edit_line(&mut t, &mut c, &key(KeyCode::Home, Mods::NONE));
        edit_line(&mut t, &mut c, &ctrl('k'));
        assert_eq!((t.as_str(), c), ("", 0));
        assert!(!edit_line(&mut t, &mut c, &key(KeyCode::Enter, Mods::NONE)));
        let (mut t, mut c) = (String::from("é日"), 1);
        paste_line(&mut t, &mut c, "ab\ncd").unwrap();
        assert!(paste_line(&mut t, &mut c, &"x".repeat(MAX_FIELD)).is_err(), "refused whole");
        assert_eq!((t.as_str(), c), ("éab日", 3));
    }

    #[test]
    fn picker_offers_new_workspace() {
        let items = vec![
            PickItem {
                label: "perso".into(),
                detail: String::new(),
                action: PickAction::SwitchWorkspace(1),
                state: None,
                dim: false,
            },
            PickItem {
                label: String::new(),
                detail: String::new(),
                action: PickAction::NewWorkspace,
                state: None,
                dim: false,
            },
        ];
        let mut p = Picker::new(PickerKind::Workspace, "ws", items);
        assert_eq!(p.filtered.len(), 1);
        p.query = "blog".into();
        p.refilter();
        assert_eq!(p.filtered.len(), 1);
        assert!(p.current().unwrap().label.contains("blog"));
        assert_eq!(p.action_keys(), &[("enter", "Create workspace")]);
    }

    #[test]
    fn picker_actions_follow_the_selected_line() {
        let mut p = Picker::new(
            PickerKind::Agent,
            "Agents",
            vec![PickItem::new(
                "Claude · work",
                "",
                PickAction::AgentLaunch { agent: "claude".into(), account: "work".into() },
            )],
        );
        assert_eq!(p.action_keys(), &[("ctrl+t", "Start in new tab"), ("ctrl+d", "Account details")]);
        p.worktree = true;
        assert_eq!(
            p.action_keys(),
            &[("ctrl+t", "Start in new tab"), ("ctrl+w", "Start in worktree"), ("ctrl+d", "Account details")]
        );
        let p = Picker::new(
            PickerKind::Accounts,
            "Accounts",
            vec![PickItem::new("Claude · work", "", PickAction::AccountDetails("id".into()))],
        );
        assert_eq!(p.action_keys(), &[("ctrl+r", "Rename"), ("ctrl+l", "Sign in"), ("ctrl+x", "Remove")]);

        for (current, what) in [(false, "Switch account"), (true, "Keep current account")] {
            let p = Picker::new(
                PickerKind::Agent,
                "Change account",
                vec![PickItem::new(
                    "codex·work",
                    "",
                    PickAction::AgentSwitch { pane: 1, agent: "codex".into(), account: "work".into(), current },
                )],
            );
            assert_eq!(p.action_keys(), &[("enter", what)]);
        }
    }

    #[test]
    fn picker_actions_require_the_line_their_handlers_use() {
        let cases = [
            (
                PickerKind::Ssh,
                PickAction::SshGroup("web".into()),
                &[("enter", "Open group"), ("ctrl+r", "Run a command on it"), ("ctrl+e", "Edit saved servers")][..],
            ),
            (
                PickerKind::Workspace,
                PickAction::SwitchWorkspace(1),
                &[("enter", "Open"), ("ctrl+x", "Close workspace")],
            ),
            (PickerKind::Workspace, PickAction::OpenDefinition("w.kdl".into()), &[("enter", "Open workspace")]),
            (PickerKind::RunbookSteps, PickAction::RunBlock(0), &[("enter", "Run step"), ("ctrl+y", "Copy step")]),
            (PickerKind::RunbookSteps, PickAction::RunbookList, &[("enter", "Back to runbooks")]),
            (
                PickerKind::Pane,
                PickAction::FilesSet { pane: 1, side: crate::files::Side::Left, target: "~".into() },
                &[("enter", "Open directory")],
            ),
            (PickerKind::Pane, PickAction::Worktree { pane: 1, op: "review".into() }, &[("enter", "Choose")]),
            (PickerKind::Workspace, PickAction::None, &[]),
        ];
        for (kind, action, expected) in cases {
            let p = Picker::new(kind, "List", vec![PickItem::new("item", "", action)]);
            assert_eq!(p.action_keys(), expected);
        }
        let p = Picker::new(PickerKind::Ssh, "Servers", vec![PickItem::new("web", "", PickAction::Ssh("web".into()))]);
        assert_eq!(
            p.action_keys().iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            ["enter", "ctrl+t", "ctrl+f", "ctrl+r", "ctrl+o", "ctrl+e", "ctrl+x"]
        );
    }

    #[test]
    fn a_picker_without_a_selected_line_offers_no_actions() {
        let mut p = Picker::new(PickerKind::Agent, "Agents", vec![]);
        assert!(p.action_keys().is_empty());
        p.items.push(PickItem::new(
            "claude",
            "",
            PickAction::AgentLaunch { agent: "claude".into(), account: "work".into() },
        ));
        p.query = "no match".into();
        p.refilter();
        assert!(p.action_keys().is_empty());
    }
}
