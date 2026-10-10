//! Interactive client modes: pickers, prompts, confirmations, help, and mouse drags.

use crate::agent::AgentState;
use crate::layout::{Border, PaneId};

pub enum Mode {
    Normal,
    /// The prefix key was pressed; the next key is a command.
    Prefix,
    /// Browsing the focused pane's scrollback.
    Scroll,
    /// Typing a search in the focused pane's scrollback.
    Search {
        query: String,
        found: bool,
    },
    Picker(Picker),
    Prompt(Prompt),
    Help {
        scroll: u16,
    },
    Confirm {
        message: String,
        action: ConfirmAction,
    },
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Prefix => "PREFIX",
            Mode::Scroll => "SCROLL",
            Mode::Search { .. } => "SEARCH",
            Mode::Picker(_) => "PICK",
            Mode::Prompt(_) => "INPUT",
            Mode::Help { .. } => "HELP",
            Mode::Confirm { .. } => "CONFIRM",
        }
    }
}

#[allow(clippy::enum_variant_names)]
pub enum ConfirmAction {
    ClosePane(PaneId),
    CloseTab {
        workspace: u32,
        tab: u32,
    },
    CloseWorkspace(u32),
    /// Run `kraal <args>` in a job pane.
    Job {
        label: String,
        args: Vec<String>,
    },
    /// Continue an agent pane's conversation on another account.
    SwitchAccount {
        pane: PaneId,
        agent: String,
        account: String,
    },
    /// Run a git shell script in a job pane.
    GitJob {
        label: String,
        script: String,
    },
    RemoveWorktree {
        pane: PaneId,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Workspace,
    Ssh,
    Pane,
    Agent,
}

#[derive(Clone)]
pub enum PickAction {
    SwitchWorkspace(u32),
    OpenDefinition(String),
    NewWorkspace,
    /// A server name or `user@host[:port]`.
    Ssh(String),
    SshGroup(String),
    FocusPane(PaneId),
    AgentLaunch {
        agent: String,
        account: String,
    },
    AgentSwitch {
        pane: PaneId,
        agent: String,
        account: String,
    },
    AgentAdd {
        agent: String,
    },
    AgentWorktree {
        agent: String,
    },
    Worktree {
        pane: PaneId,
        op: String,
    },
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

pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub query: String,
    pub items: Vec<PickItem>,
    /// Indices into `items` matching the query, best first.
    pub filtered: Vec<usize>,
    pub selected: usize,
}

impl Picker {
    pub fn new(kind: PickerKind, title: &str, items: Vec<PickItem>) -> Picker {
        let mut p =
            Picker { kind, title: title.into(), query: String::new(), items, filtered: Vec::new(), selected: 0 };
        p.refilter();
        p
    }

    pub fn refilter(&mut self) {
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| !matches!(it.action, PickAction::NewWorkspace))
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
    /// The value is the new account's name.
    AddAccount {
        agent: String,
    },
    /// The value is the worktree's name.
    NewWorktree {
        agent: String,
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
}

pub struct Prompt {
    pub title: String,
    pub value: String,
    pub action: PromptAction,
}

impl Prompt {
    pub fn new(title: &str, value: &str, action: PromptAction) -> Prompt {
        Prompt { title: title.into(), value: value.into(), action }
    }
}

/// A mouse drag in progress.
pub enum Drag {
    Border(Border),
    Select {
        pane: PaneId,
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
    },
}

/// Subsequence fuzzy match. `None` if `query` does not match; higher is better.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
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
    }
}
