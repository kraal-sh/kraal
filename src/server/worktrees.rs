//! Git in the server: branch and changes of each pane's directory (from a background thread),
//! and worktrees that give an agent its own branch, with review, pull request and cleanup.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::Sender;

use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Event, Server};
use crate::git::{self, GitInfo, Worktree};
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::Placement;
use crate::transfer::sh_quote;

/// How a git job pane ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobEnd {
    /// Closes a few seconds after succeeding; stays open with the error otherwise.
    CloseOnSuccess,
    /// Closes as soon as it exits (a pager the user quits).
    CloseOnExit,
}

/// Start the thread that runs `git status` for directories the server asks about.
pub fn spawn_git_worker(tx: Sender<Event>) -> Sender<PathBuf> {
    let (req_tx, req_rx) = crossbeam_channel::unbounded::<PathBuf>();
    let _ = std::thread::Builder::new().name("git".into()).spawn(move || {
        for dir in req_rx {
            let info = git::status(&dir);
            if tx.send(Event::Git(dir, info.map(Box::new))).is_err() {
                break;
            }
        }
    });
    req_tx
}

impl Server {
    /// Ask the git thread about the directories of local terminal panes, every few seconds.
    pub(super) fn refresh_git(&mut self) {
        self.last_git_scan = Instant::now();
        let dirs: Vec<PathBuf> = self
            .panes
            .values()
            .filter(|p| p.is_terminal() && p.ssh.is_none() && p.exit_code.is_none())
            .filter_map(|p| p.current_cwd())
            .collect();
        for d in dirs {
            if self.git_pending.insert(d.clone()) {
                let _ = self.git_requests.send(d);
            }
        }
    }

    pub(super) fn git_result(&mut self, dir: PathBuf, info: Option<Box<GitInfo>>) {
        self.git_pending.remove(&dir);
        let info = info.map(|b| *b);
        if self.git.get(&dir) != Some(&info) {
            self.git.insert(dir, info);
            self.mark_dirty();
        }
    }

    /// Branch and changes of a pane's directory, if known.
    pub fn pane_git(&self, pane: PaneId) -> Option<&GitInfo> {
        let p = self.panes.get(&pane)?;
        if p.ssh.is_some() {
            return None;
        }
        self.git.get(&p.current_cwd()?)?.as_ref()
    }

    /// The worktree a pane works in: the one kraal created for it, or found from its directory.
    pub fn pane_worktree(&self, pane: PaneId) -> Option<Worktree> {
        let p = self.panes.get(&pane)?;
        if let Some(w) = &p.spec.worktree {
            return Some(w.clone());
        }
        let info = self.pane_git(pane)?;
        let name = info.branch.strip_prefix("kraal/")?;
        let repo = git::main_checkout(&info.root)?;
        Some(Worktree { repo, path: info.root.clone(), branch: format!("kraal/{name}"), base: String::new() })
    }

    /// Create a worktree from the focused pane's repository and start `agent` in it, in a new tab.
    pub(super) fn agent_in_worktree(&mut self, client: ClientId, agent: &str, name: &str) -> Result<PaneId> {
        let ws = self.clients.get(&client).map(|c| c.workspace).context("no client")?;
        let cwd = self
            .client_focus(client)
            .and_then(|p| self.panes.get(&p))
            .and_then(|p| p.current_cwd())
            .context("no directory to take the repository from")?;
        let wt = git::add_worktree(&cwd, name, None)?;
        let wi = self.ws_index(ws).context("no workspace")?;
        let ws_name = self.workspaces[wi].name.clone();
        let account = self
            .accounts
            .choose(agent, Some(&ws_name), Some(&wt.path), &self.usage, crate::accounts::now())
            .context("no account for this agent")?;
        let mut spec = self.agent_spec(&account, Some(wt.path.clone()), None, &[])?;
        spec.worktree = Some(wt.clone());
        let id = self.spawn_pane_in(ws, None, spec, Placement::Tab, Some(format!("{agent} {name}")))?;
        if let Some((wi, ti)) = self.workspace_of(id) {
            self.workspaces[wi].tabs[ti].name = name.to_string();
        }
        self.mark_persist();
        Ok(id)
    }

    /// What can be done with a worktree: review, commit, pull request, merge, remove.
    pub(super) fn worktree_picker(&self, pane: PaneId) -> Option<Picker> {
        let w = self.pane_worktree(pane)?;
        let base = if w.base.is_empty() { "the base branch".to_string() } else { w.base.clone() };
        let item = |label: &str, detail: String, op: &str| PickItem {
            label: label.into(),
            detail,
            action: PickAction::Worktree { pane, op: op.into() },
            state: None,
            dim: false,
        };
        let mut items = vec![
            item("Review changes", format!("diff against {base}, uncommitted changes included"), "review"),
            item("Commit everything", "stage all changes and commit (you write the message)".into(), "commit"),
        ];
        if crate::transfer::have_program("gh") {
            items.push(item("Open a pull request", format!("push {} and run gh pr create", w.branch), "pr"));
        }
        items.push(item("Merge into base", format!("merge {} into {base} in {}", w.branch, w.repo.display()), "merge"));
        items.push(item("Remove worktree", format!("delete {} and its branch", w.path.display()), "remove"));
        Some(Picker::new(PickerKind::Pane, &format!("Worktree {}", w.name()), items))
    }

    pub(super) fn worktree_op(&mut self, client: ClientId, pane: PaneId, op: &str) -> Mode {
        let Some(w) = self.pane_worktree(pane) else { return Mode::Normal };
        let path = sh_quote(&w.path.display().to_string());
        let repo = sh_quote(&w.repo.display().to_string());
        let base = if w.base.is_empty() {
            "$(git -C ".to_string() + &repo + " rev-parse --abbrev-ref HEAD)"
        } else {
            sh_quote(&w.base)
        };
        let branch = sh_quote(&w.branch);
        match op {
            "review" => {
                // One pager view (summary, then the diff); quitting it with q closes the pane.
                // LESS=-R keeps less open even when the diff fits on one screen.
                let script = format!("cd {path} && LESS=-R git diff --stat -p {base}");
                self.git_job(client, "review", script, JobEnd::CloseOnExit);
                Mode::Normal
            }
            "commit" => Mode::Prompt(super::ui::Prompt::new(
                &format!("Commit message for {}", w.name()),
                "",
                super::ui::PromptAction::Commit { pane },
            )),
            "pr" => {
                let script = format!("cd {path} && git push -u origin {branch} && gh pr create --fill --head {branch}");
                self.git_job(client, "pull request", script, JobEnd::CloseOnSuccess);
                Mode::Normal
            }
            "merge" => Mode::Confirm {
                message: format!("Merge {} into the base branch of {}?", w.branch, w.repo.display()),
                action: ConfirmAction::GitJob {
                    label: "merge".into(),
                    script: format!("cd {repo} && git checkout {base} && git merge --no-ff {branch}"),
                },
            },
            "remove" => Mode::Confirm {
                message: format!("Remove the worktree {} and delete {}?", w.path.display(), w.branch),
                action: ConfirmAction::RemoveWorktree { pane },
            },
            _ => Mode::Normal,
        }
    }

    pub(super) fn commit_worktree(&mut self, client: ClientId, pane: PaneId, message: &str) {
        let Some(w) = self.pane_worktree(pane) else { return };
        if message.trim().is_empty() {
            self.toast(client, "empty commit message");
            return;
        }
        let script = format!(
            "cd {} && git add -A && git commit -m {}",
            sh_quote(&w.path.display().to_string()),
            sh_quote(message)
        );
        self.git_job(client, "commit", script, JobEnd::CloseOnSuccess);
    }

    pub(super) fn remove_worktree(&mut self, client: ClientId, pane: PaneId) {
        let Some(w) = self.pane_worktree(pane) else { return };
        match git::remove_worktree(&w, true) {
            Ok(()) => {
                self.close_pane(pane);
                self.toast(client, format!("removed {}", w.path.display()));
            }
            Err(e) => self.toast(client, format!("{e:#}")),
        }
    }

    /// Run a git shell script in a floating pane.
    pub(super) fn git_job(&mut self, client: ClientId, label: &str, script: String, end: JobEnd) {
        let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return };
        let spec = PaneSpec {
            command: vec!["sh".into(), "-c".into(), script],
            close_on_success: end == JobEnd::CloseOnSuccess,
            close_on_exit: end == JobEnd::CloseOnExit,
            ..Default::default()
        };
        if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(format!("⎇ {label}"))) {
            self.toast(client, format!("{e:#}"));
        }
        // Branch states change after these: look again soon.
        self.last_git_scan = Instant::now() - Duration::from_secs(10);
    }

    /// Worktree items for the agent picker: start an agent on a new branch of this repository.
    pub(super) fn worktree_items(&self, client: ClientId) -> Vec<PickItem> {
        let in_repo = self.client_focus(client).and_then(|p| self.pane_git(p)).is_some();
        if !in_repo {
            return Vec::new();
        }
        crate::accounts::AGENTS
            .iter()
            .map(|k| PickItem {
                label: format!("+ {} in a new worktree", k.name),
                detail: "its own branch (kraal/<name>) and directory, next to this repository".into(),
                action: PickAction::AgentWorktree { agent: k.name.to_string() },
                state: None,
                dim: true,
            })
            .collect()
    }
}
