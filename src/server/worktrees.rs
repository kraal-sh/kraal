//! Git in the server: branch and changes of each pane's directory (from a background thread),
//! and worktrees that give an agent its own branch, prepared before it starts, with review,
//! pull request and cleanup.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::Sender;

use super::dirwatch::SOON_EVERY;
use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Event, EventSender, Server};
use crate::git::{self, GitInfo, Worktree, WorktreeState};
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::Placement;
use crate::shell;
use crate::worktree_setup::{self, Plan, Prepared};

/// A worktree about to be made for an agent, while the user is asked to trust its setup.
pub struct NewWorktree {
    ws: u32,
    agent: String,
    plan: Plan,
}

/// How a git job pane ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobEnd {
    /// Closes a few seconds after succeeding; stays open with the error otherwise.
    CloseOnSuccess,
    /// Closes as soon as it exits (a pager the user quits).
    CloseOnExit,
}

/// Start the thread that runs `git status` for directories the server asks about.
pub fn spawn_git_worker(tx: EventSender) -> std::io::Result<Sender<PathBuf>> {
    let (req_tx, req_rx) = crossbeam_channel::unbounded::<PathBuf>();
    crate::threads::named("git").spawn(move || {
        for dir in req_rx {
            let info = git::status(&dir);
            // Account rules see a worktree as its repository: found here, off the event loop
            // (and remembered for a directory outside any repository too).
            git::project_path(&dir);
            if tx.send(Event::Git(dir, info.map(Box::new))).is_err() {
                break;
            }
        }
    })?;
    Ok(req_tx)
}

impl Server {
    /// Ask the git thread about the directories of local terminal panes that are due (see
    /// [`super::dirwatch::DirWatch::periodic`]), and forget those no pane is in any more.
    pub(super) fn refresh_git(&mut self, now: Instant) {
        // Each local pane's directory, with the time of its last output while its program runs.
        let panes: Vec<(PathBuf, Option<Instant>)> = self
            .panes
            .values()
            .filter(|p| p.ssh.is_none())
            .filter_map(|p| {
                let running = p.is_terminal() && p.exit_code.is_none();
                Some((p.current_cwd()?, running.then_some(p.last_output)))
            })
            .collect();
        let mut live = std::collections::HashSet::new();
        let mut shown = std::collections::HashSet::new();
        for (dir, output) in panes {
            if let Some(output) = output {
                if self.dirwatch.periodic(&dir, output, now) {
                    self.ask_git(dir.clone(), false);
                }
                live.insert(dir.clone());
            }
            shown.insert(dir);
        }
        self.dirwatch.retain(&live);
        forget_git_unshown(&mut self.git, &shown);
    }

    /// Ask the git thread about `dir` (its branch, and the repository account rules see it in),
    /// unless it is already asked.
    pub(super) fn ask_git_about(&mut self, dir: PathBuf) {
        self.ask_git(dir, false);
    }

    /// Ask about `dir`; when it is being asked already and `again` (something changed since),
    /// once more after the answer.
    fn ask_git(&mut self, dir: PathBuf, again: bool) {
        if self.dirwatch.asking(&dir, again, Instant::now()) {
            let _ = self.git_requests.send(dir);
        }
    }

    /// Something may have changed in `dir` (a command ended there): ask git about it soon.
    pub(super) fn git_soon(&mut self, dir: &Path) {
        self.dirwatch.soon(dir, Duration::ZERO, SOON_EVERY, Instant::now());
    }

    /// Ask soon about every directory panes are in (a pane's program ended: a git job, a
    /// commit, a checkout).
    pub(super) fn git_soon_everywhere(&mut self) {
        let now = Instant::now();
        let dirs: Vec<PathBuf> = self.dirwatch.dirs().cloned().collect();
        for d in dirs {
            self.dirwatch.soon(&d, Duration::ZERO, SOON_EVERY, now);
        }
    }

    /// The user is back (the terminal got the focus, a client attached): what it shows of git
    /// may have changed meanwhile, from elsewhere. The directories of its workspace's panes are
    /// asked about soon.
    pub(super) fn git_soon_for(&mut self, client: ClientId) {
        let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return };
        let Some(wi) = self.ws_index(ws) else { return };
        let dirs: Vec<PathBuf> = self.workspaces[wi]
            .panes()
            .iter()
            .filter_map(|p| self.panes.get(p))
            .filter(|p| p.is_terminal() && p.ssh.is_none() && p.exit_code.is_none())
            .filter_map(|p| p.current_cwd())
            // Those never asked about are at the next tick.
            .filter(|d| self.git.contains_key(d))
            .collect();
        let now = Instant::now();
        for d in dirs {
            self.dirwatch.soon(&d, Duration::ZERO, SOON_EVERY, now);
        }
    }

    /// Ask about the directories whose time came (see [`Server::git_soon`]).
    pub(super) fn git_due(&mut self, now: Instant) {
        if self.dirwatch.next().is_some_and(|t| t <= now) {
            for dir in self.dirwatch.take_due(now) {
                self.ask_git(dir, true);
            }
        }
    }

    pub(super) fn git_result(&mut self, dir: PathBuf, info: Option<Box<GitInfo>>) {
        let info = info.map(|b| *b);
        let watch = info.as_ref().map(|i| i.watch.as_slice()).unwrap_or_default();
        self.dirwatch.answered(&dir, info.is_some(), watch, Instant::now());
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

    /// The worktree a pane works in: the one kraal created for it, or found from its directory
    /// (from what the git thread said about it: git never runs on the event loop).
    pub fn pane_worktree(&self, pane: PaneId) -> Option<Worktree> {
        let p = self.panes.get(&pane)?;
        if let Some(w) = &p.spec.worktree {
            return Some(w.clone());
        }
        worktree_of(self.pane_git(pane)?)
    }

    /// Give a pane opened in a worktree that worktree's variables (its name, number and port
    /// offset), unless it has them.
    pub(super) fn add_worktree_env(&self, spec: &mut PaneSpec) {
        if spec.env.iter().any(|(k, _)| k == "KRAAL_WORKTREE") {
            return;
        }
        let found = match &spec.worktree {
            Some(w) => Some((w.name(), w.index)),
            None => spec.cwd.as_deref().and_then(|cwd| self.worktree_at(cwd)),
        };
        if let Some((name, index)) = found {
            spec.env.extend(worktree_setup::env(&name, index, &self.config.worktrees));
        }
    }

    /// The kraal worktree `cwd` is in, and its number, as known without asking git: what git
    /// said about that very directory, else from a pane kraal started for one, or from what git
    /// said about a pane's directory.
    fn worktree_at(&self, cwd: &Path) -> Option<(String, u32)> {
        // Its branch's name, or with another checked out (a detached HEAD), its own.
        let named = |g: &GitInfo| {
            let name = match g.branch.strip_prefix("kraal/") {
                Some(name) => name.to_string(),
                None => g.root.file_name()?.to_string_lossy().into_owned(),
            };
            Some((name, g.worktree_index))
        };
        if let Some(Some(g)) = self.git.get(cwd)
            && g.worktree_index > 0
        {
            return named(g);
        }
        let started = self
            .panes
            .values()
            .filter_map(|p| p.spec.worktree.as_ref())
            .find(|w| w.index > 0 && self.dir_within(cwd, &w.path))
            .map(|w| (w.name(), w.index));
        started.or_else(|| {
            self.git
                .values()
                .flatten()
                .filter(|g| g.worktree_index > 0 && self.dir_within(cwd, &g.root))
                .find_map(named)
        })
    }

    /// Whether `dir` is `root` or inside it, `root` as git gives paths (symbolic links resolved):
    /// `dir` as it is, or as the git thread found it resolved. Nothing is asked of the disk here.
    fn dir_within(&self, dir: &Path, root: &Path) -> bool {
        dir.starts_with(root) || self.git.get(dir).and_then(Option::as_ref).is_some_and(|g| g.real.starts_with(root))
    }

    /// Whether the variables of a pane to open in `cwd` are still to be found: it may be in a
    /// kraal worktree git has not been asked about yet. Then git is asked first, off the loop.
    pub(super) fn worktree_unknown(&self, cwd: &Path) -> bool {
        worktree_setup::in_worktrees_dir(cwd) && !self.git.contains_key(cwd) && self.worktree_at(cwd).is_none()
    }

    /// The worktree a pane works in, for the sidebar: cheap, from what it was started with.
    pub(super) fn pane_worktree_name(&self, pane: PaneId) -> Option<String> {
        let spec = &self.panes.get(&pane)?.spec;
        spec.worktree
            .as_ref()
            .map(Worktree::name)
            .or_else(|| spec.env.iter().find(|(k, _)| k == "KRAAL_WORKTREE").map(|(_, v)| v.clone()))
    }

    /// Panes working inside a worktree's directory.
    fn panes_in(&self, w: &Worktree) -> Vec<PaneId> {
        self.panes
            .values()
            .filter(|p| {
                p.spec.worktree.as_ref().is_some_and(|pw| pw.path == w.path)
                    || (p.ssh.is_none() && p.current_cwd().is_some_and(|c| self.dir_within(&c, &w.path)))
            })
            .map(|p| p.id)
            .collect()
    }

    /// Create a worktree from the focused pane's repository, prepare it as the repository and
    /// the config say, then start `agent` in it in a new tab. Git runs in the background (it
    /// can be slow); the repository's setup commands are approved first.
    pub(super) fn agent_in_worktree(&mut self, client: ClientId, agent: &str, name: &str) -> Result<()> {
        let ws = self.clients.get(&client).map(|c| c.workspace).context("no client")?;
        let cwd = self
            .client_focus(client)
            .and_then(|p| self.panes.get(&p))
            .filter(|p| p.ssh.is_none())
            .and_then(|p| p.current_cwd())
            .context("no local directory to take the repository from")?;
        anyhow::ensure!(git::valid_worktree_name(name), "invalid worktree name {name:?} (letters, digits, - _ .)");
        let user = worktree_setup::Setup::of_user(&self.config.worktrees);
        let (agent, name) = (agent.to_string(), name.to_string());
        self.toast(client, format!("creating worktree {name}…"));
        self.background(
            move || Plan::new(&cwd, &name, None, user, true),
            move |server, plan| match plan {
                Ok(plan) if plan.needs_trust() => {
                    let question = plan.trust_question();
                    let new = Box::new(NewWorktree { ws, agent, plan });
                    server.set_mode_if_idle(client, Mode::confirm(question, ConfirmAction::TrustWorktreeSetup(new)));
                }
                Ok(plan) => server.create_worktree(client, NewWorktree { ws, agent, plan }),
                Err(e) => server.toast(client, format!("{e:#}")),
            },
        );
        Ok(())
    }

    /// The answer to the question about a repository's worktree setup: a yes trusts this
    /// version of it and makes the worktree; a no makes nothing.
    pub(super) fn confirm_worktree_setup(&mut self, client: ClientId, new: NewWorktree, yes: bool) {
        if !yes {
            self.toast(client, format!("the worktree {} was not created", new.plan.name));
            return;
        }
        if let Err(e) = new.plan.trust() {
            self.warnings.push(format!("{e:#}"));
        }
        self.create_worktree(client, new);
    }

    fn create_worktree(&mut self, client: ClientId, new: NewWorktree) {
        let NewWorktree { ws, agent, plan } = new;
        self.background(
            move || plan.create(),
            move |server, created| {
                let result = created.and_then(|p| server.start_agent_in(ws, &agent, p));
                if let Err(e) = result {
                    server.toast(client, format!("{e:#}"));
                }
            },
        );
    }

    /// Start `agent` in its new worktree, in a tab named after it; the worktree's setup runs in
    /// its pane first.
    fn start_agent_in(&mut self, ws: u32, agent: &str, prepared: Prepared) -> Result<()> {
        let wi = self.ws_index(ws).context("the workspace was closed")?;
        let ws_name = self.workspaces[wi].name.clone();
        let wt = prepared.worktree.clone();
        let name = wt.name();
        // Account rules are written for the repository, not for its worktrees' directories.
        let account = self
            .accounts
            .choose(agent, Some(&ws_name), Some(&wt.repo), &self.usage, crate::accounts::now())
            .context("no account for this agent")?;
        let mut spec = self.agent_spec(&account, Some(wt.path.clone()), None, &[])?;
        spec.worktree = Some(wt.clone());
        let title = Some(format!("{agent} {name}"));
        let id = match prepared.script() {
            Some(script) => {
                let setup = PaneSpec {
                    command: vec!["sh".into(), "-c".into(), script],
                    cwd: Some(wt.path.clone()),
                    worktree: Some(wt),
                    ..Default::default()
                };
                self.spawn_pane_after(ws, setup, spec, Placement::Tab, title)?
            }
            None => self.spawn_pane_in(ws, None, spec, Placement::Tab, title)?,
        };
        if let Some((wi, ti)) = self.workspace_of(id) {
            self.workspaces[wi].tabs[ti].name = name;
        }
        self.mark_persist();
        Ok(())
    }

    /// What can be done with a worktree: review, commit, pull request, merge, remove; and in any
    /// checkout of a repository, remove its merged worktrees.
    pub(super) fn worktree_picker(&self, pane: PaneId) -> Option<Picker> {
        let item = |label: &str, detail: String, op: &str| PickItem {
            label: label.into(),
            detail,
            action: PickAction::Worktree { pane, op: op.into() },
            state: None,
            dim: false,
        };
        let prune = item(
            "Remove merged worktrees",
            "those with nothing to lose: no changes, every commit (or its changes) in their base".into(),
            "prune",
        );
        let Some(w) = self.pane_worktree(pane) else {
            let root = &self.pane_git(pane)?.root;
            let repo = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Some(Picker::new(PickerKind::Pane, &format!("Worktrees of {repo}"), vec![prune]));
        };
        let base = if w.base.is_empty() { "the base branch".to_string() } else { w.base.clone() };
        let mut items = vec![
            item("Review changes", format!("diff against {base}, uncommitted changes included"), "review"),
            item("Commit everything", "stage all changes and commit (you write the message)".into(), "commit"),
        ];
        if crate::transfer::have_program("gh") {
            items.push(item("Open a pull request", format!("push {} and run gh pr create", w.branch), "pr"));
        }
        items.push(item("Merge into base", format!("merge {} into {base} in {}", w.branch, w.repo.display()), "merge"));
        items.push(item("Remove worktree", format!("delete {} and its branch", w.path.display()), "remove"));
        items.push(prune);
        Some(Picker::new(PickerKind::Pane, &format!("Worktree {}", w.name()), items))
    }

    pub(super) fn worktree_op(&mut self, client: ClientId, pane: PaneId, op: &str) -> Mode {
        if op == "prune" {
            let dir = self.pane_git(pane).map(|g| g.root.clone()).or_else(|| self.pane_worktree(pane).map(|w| w.repo));
            let Some(dir) = dir else { return Mode::Normal };
            // What removing each would lose is asked first (git may take a moment).
            self.background(
                move || git::worktree_states(&dir),
                move |server, states| match states {
                    Ok(states) => server.ask_prune_worktrees(client, states),
                    Err(e) => server.toast(client, format!("{e:#}")),
                },
            );
            return Mode::Normal;
        }
        let Some(w) = self.pane_worktree(pane) else { return Mode::Normal };
        let path = shell::quote(&w.path.display().to_string());
        let repo = shell::quote(&w.repo.display().to_string());
        let base = if w.base.is_empty() {
            "$(git -C ".to_string() + &repo + " rev-parse --abbrev-ref HEAD)"
        } else {
            shell::quote(&w.base)
        };
        let branch = shell::quote(&w.branch);
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
                self.git_job(client, "pull request", pr_script(&w), JobEnd::CloseOnSuccess);
                Mode::Normal
            }
            "merge" => Mode::confirm(
                format!("Merge {} into the base branch of {}?", w.branch, w.repo.display()),
                ConfirmAction::GitJob {
                    label: "merge".into(),
                    script: format!("cd {repo} && git checkout {base} && git merge --no-ff {branch}"),
                },
            ),
            "remove" => {
                // Find out what removing would lose before asking (git may take a moment).
                let check = w.clone();
                self.background(
                    move || git::worktree_state(&check),
                    move |server, state| server.ask_remove_worktree(client, w, state),
                );
                Mode::Normal
            }
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
            shell::quote(&w.path.display().to_string()),
            shell::quote(message)
        );
        self.git_job(client, "commit", script, JobEnd::CloseOnSuccess);
    }

    fn ask_remove_worktree(&mut self, client: ClientId, w: Worktree, state: Result<git::WorktreeState>) {
        let state = match state {
            Ok(s) => s,
            Err(e) => {
                self.toast(client, format!("{e:#}"));
                return;
            }
        };
        let panes = self.panes_in(&w);
        let mut lines = vec![format!("Remove the worktree {} and its branch {}?", w.path.display(), w.branch)];
        if !state.loses_nothing() {
            lines.push(format!("It has {}: they will be lost.", state.describe()));
        } else if state.unmerged > 0 {
            lines.push("Its commits are not in the history of its base, but all their changes are.".into());
        }
        if !panes.is_empty() {
            let names: Vec<String> = panes.iter().map(|p| self.pane_label(*p)).collect();
            lines.push(format!(
                "Closes {}: {}",
                if panes.len() == 1 { "1 pane" } else { "these panes" },
                names.join(", ")
            ));
        }
        self.set_mode_if_idle(
            client,
            Mode::confirm(lines.join("\n"), ConfirmAction::RemoveWorktrees(vec![(w, state)])),
        );
    }

    /// Offer to remove the worktrees that would lose no work (no changes, every commit or all
    /// its changes in their base), saying which panes that closes, which ignored files go with
    /// them, and why the others stay.
    fn ask_prune_worktrees(&mut self, client: ClientId, states: Vec<(Worktree, Result<WorktreeState, String>)>) {
        let (mut merged, mut kept) = (Vec::new(), Vec::new());
        for (w, state) in states {
            match state {
                Ok(s) if s.is_clean() => merged.push((w, s)),
                Ok(s) => kept.push(format!("{} ({})", w.name(), s.describe())),
                Err(e) => kept.push(format!("{} ({e})", w.name())),
            }
        }
        if merged.is_empty() {
            let why = if kept.is_empty() { String::new() } else { format!("; kept: {}", kept.join(", ")) };
            self.toast(client, format!("no merged worktree to remove{why}"));
            return;
        }
        let mut lines = vec![format!(
            "Remove {} and {}? They have no changes, and their base has all their commits, or all their changes:",
            plural(merged.len(), "merged worktree"),
            if merged.len() == 1 { "its branch" } else { "their branches" }
        )];
        for (w, s) in &merged {
            let how = if s.unmerged > 0 { "  (its changes are in its base)" } else { "" };
            lines.push(format!("  {}  {}{how}", w.name(), crate::paths::shorten_home(&w.path)));
        }
        let ignored: Vec<String> = merged
            .iter()
            .filter(|(_, s)| !s.ignored.is_empty())
            .map(|(w, s)| format!("{} ({})", w.name(), s.ignored_names()))
            .collect();
        if !ignored.is_empty() {
            lines.push(format!(
                "Their ignored files that the main checkout does not have are deleted with them: {}",
                ignored.join(", ")
            ));
        }
        let mut panes: Vec<PaneId> = Vec::new();
        for p in merged.iter().flat_map(|(w, _)| self.panes_in(w)) {
            if !panes.contains(&p) {
                panes.push(p);
            }
        }
        if !panes.is_empty() {
            let names: Vec<String> = panes.iter().map(|p| self.pane_label(*p)).collect();
            lines.push(format!(
                "Closes {}: {}",
                if panes.len() == 1 { "1 pane" } else { "these panes" },
                names.join(", ")
            ));
        }
        if !kept.is_empty() {
            lines.push(format!("Kept: {}", kept.join(", ")));
        }
        self.set_mode_if_idle(client, Mode::confirm(lines.join("\n"), ConfirmAction::RemoveWorktrees(merged)));
    }

    /// Remove the worktrees the user confirmed, as they were then: close their panes, wait for
    /// their processes to end, check nothing changed since the question (else ask again),
    /// remove.
    pub(super) fn remove_worktrees(&mut self, client: ClientId, list: Vec<(Worktree, WorktreeState)>) {
        let Some((first, _)) = list.first() else { return };
        let repo = first.repo.clone();
        let mut in_tree: Vec<PaneId> = Vec::new();
        for p in list.iter().flat_map(|(w, _)| self.panes_in(w)) {
            if !in_tree.contains(&p) {
                in_tree.push(p);
            }
        }
        let pids: Vec<u32> = in_tree.iter().filter_map(|p| self.panes.get(p)?.pid()).collect();
        // Closing the last panes would end the session before the removal ran: a shell in the
        // repository takes their place.
        if !in_tree.is_empty()
            && self.panes.keys().all(|p| in_tree.contains(p))
            && let Some((wi, _)) = self.workspace_of(in_tree[0])
        {
            let ws = self.workspaces[wi].id;
            let spec = PaneSpec { cwd: Some(repo), ..Default::default() };
            if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Tab, None) {
                self.toast(client, format!("{e:#}: nothing was removed"));
                return;
            }
        }
        for p in in_tree {
            self.close_pane(p);
        }
        self.background(
            move || remove_unchanged(list, &pids),
            move |server, (removed, errors): (Vec<Worktree>, Vec<String>)| {
                let mut parts = Vec::new();
                match removed.as_slice() {
                    [] => {}
                    [w] => parts.push(format!("removed {} and {}", w.path.display(), w.branch)),
                    many => {
                        let names: Vec<String> = many.iter().map(Worktree::name).collect();
                        parts.push(format!("removed {} worktrees: {}", many.len(), names.join(", ")));
                    }
                }
                parts.extend(errors);
                server.toast(client, parts.join("; "));
            },
        );
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
        // Branch states change after these: git is asked again when its pane's program ends.
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

/// Remove each worktree of `list` once the programs `pids` of its panes are gone, if it is still
/// as the user was shown; the worktrees removed, and why the others were not.
fn remove_unchanged(list: Vec<(Worktree, WorktreeState)>, pids: &[u32]) -> (Vec<Worktree>, Vec<String>) {
    if !crate::exec::wait_gone(pids, Duration::from_secs(5)) {
        let names: Vec<String> = list.iter().map(|(w, _)| w.name()).collect();
        let error =
            format!("programs of {} are still running after their panes closed; nothing was removed", names.join(", "));
        return (Vec::new(), vec![error]);
    }
    let unchanged = |w: &Worktree, state: &WorktreeState| -> Result<()> {
        // Whatever its panes started in another process group (a build, a test watcher) and
        // that ignored the hang-up may still write there.
        let busy = crate::process::working_in(&w.path);
        if !busy.is_empty() {
            let names: Vec<String> = busy.iter().map(|(pid, name)| format!("{name} ({pid})")).collect();
            anyhow::bail!(
                "{} still runs or writes in {}: stop it, then remove the worktree; nothing was removed",
                names.join(", "),
                w.name()
            );
        }
        let now = git::worktree_state(w)?;
        anyhow::ensure!(
            now.same_work(state),
            "{} changed since you confirmed ({}); ask again",
            w.name(),
            now.describe()
        );
        Ok(())
    };
    let mut errors = Vec::new();
    let mut ready = Vec::new();
    for (w, state) in list {
        match unchanged(&w, &state) {
            Ok(()) => ready.push((w, state)),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    // Something started elsewhere may still be writing there: it must have settled.
    if !ready.is_empty() {
        std::thread::sleep(Duration::from_millis(500));
    }
    let mut removed = Vec::new();
    for (w, state) in ready {
        let result = (|| -> Result<()> {
            let now = git::worktree_state(&w)?;
            anyhow::ensure!(
                now.same_work(&state),
                "{} is still changing (something outside its panes writes there); nothing was removed",
                w.name()
            );
            // What would be lost was shown and accepted; nothing else is lost: the branch goes
            // only at the commit checked now.
            git::remove_checked(&w, &now)
        })();
        match result {
            Ok(()) => removed.push(w),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    (removed, errors)
}

/// Forget what git said about directories no pane is in any more (`shown` are those panes are
/// in): kept, it would grow with every directory a shell ever went to. One a pane goes back to
/// is asked about again.
fn forget_git_unshown(
    git: &mut std::collections::HashMap<PathBuf, Option<GitInfo>>,
    shown: &std::collections::HashSet<PathBuf>,
) {
    git.retain(|dir, _| shown.contains(dir));
}

/// The kraal worktree git described with `info`, if it is one.
fn worktree_of(info: &GitInfo) -> Option<Worktree> {
    if !info.branch.starts_with("kraal/") || info.main.as_os_str().is_empty() {
        return None;
    }
    Some(Worktree {
        repo: info.main.clone(),
        path: info.root.clone(),
        branch: info.branch.clone(),
        base: info.base.clone(),
        index: info.worktree_index,
    })
}

/// `1 merged worktree`, `2 merged worktrees`.
fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// The script that pushes a worktree's branch and opens its pull request, against the branch
/// the worktree started from (a commit id cannot be a PR base). Nothing runs anywhere else when
/// the worktree's directory is gone: pushing from the directory the job started in would
/// publish another repository's branch.
fn pr_script(w: &Worktree) -> String {
    let path = shell::quote(&w.path.display().to_string());
    let branch = shell::quote(&w.branch);
    let is_commit = w.base.len() == 40 && w.base.bytes().all(|b| b.is_ascii_hexdigit());
    let mut script = format!("cd {path} || exit 1\n");
    let mut pr_base = String::new();
    if !w.base.is_empty() && !is_commit {
        script.push_str(&pr_base_script(&w.base));
        script.push('\n');
        pr_base = " --base \"$base\"".to_string();
    }
    script.push_str(&format!("git push -u origin {branch} && gh pr create --fill --head {branch}{pr_base}\n"));
    script
}

/// Shell code setting `$base` to the branch a pull request goes into, from the base a worktree
/// was made from: `origin/main` (a remote-tracking branch) is `main` on the server.
fn pr_base_script(base: &str) -> String {
    format!(
        "base={b}; full=$(git rev-parse --symbolic-full-name \"$base\" 2>/dev/null); \
         case \"$full\" in refs/remotes/*/*) base=${{full#refs/remotes/*/}} ;; esac",
        b = shell::quote(base)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_git_said_goes_with_the_last_pane_in_its_directory() {
        let dir = |i: usize| PathBuf::from(format!("/x/{i}"));
        let mut git: std::collections::HashMap<PathBuf, Option<GitInfo>> = std::collections::HashMap::new();
        // A shell that went through a thousand directories, two panes left in two of them.
        for i in 0..1000 {
            git.insert(dir(i), None);
        }
        let shown: std::collections::HashSet<PathBuf> = [dir(3), dir(999)].into_iter().collect();
        forget_git_unshown(&mut git, &shown);
        let mut kept: Vec<PathBuf> = git.into_keys().collect();
        kept.sort();
        assert_eq!(kept, vec![dir(3), dir(999)]);
    }

    #[test]
    fn pull_requests_go_into_the_branch_on_the_server() {
        let dir = std::env::temp_dir().join(format!("kraal-pr-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&["update-ref", "refs/remotes/origin/release", "HEAD"]);
        let base = |b: &str| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{} && printf %s \"$base\"", pr_base_script(b)))
                .current_dir(&dir)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(base("origin/release"), "release");
        assert_eq!(base("main"), "main");
        assert_eq!(base("it's"), "it's", "an odd name stays as it is");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pull_request_is_never_pushed_from_another_directory() {
        let dir = std::env::temp_dir().join(format!("kraal-pr-cd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // git and gh leave a trace of where they ran.
        for tool in ["git", "gh"] {
            crate::testing::write_script(&bin.join(tool), &format!("#!/bin/sh\npwd >> \"{}/ran\"\n", dir.display()));
        }
        let w = Worktree {
            repo: dir.join("repo"),
            path: dir.join("gone"),
            branch: "kraal/x".into(),
            base: "origin/main".into(),
            index: 1,
        };
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(pr_script(&w))
            .current_dir(&dir)
            .env("PATH", &path)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
        assert!(!dir.join("ran").exists(), "nothing ran in the directory the job started in");
        // With the directory there, the push and the pull request run in it.
        std::fs::create_dir_all(&w.path).unwrap();
        let status =
            std::process::Command::new("sh").arg("-c").arg(pr_script(&w)).current_dir(&dir).env("PATH", &path).status();
        assert!(status.unwrap().success());
        let ran = std::fs::read_to_string(dir.join("ran")).unwrap();
        assert!(ran.lines().all(|l| l.ends_with("/gone")), "{ran}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
