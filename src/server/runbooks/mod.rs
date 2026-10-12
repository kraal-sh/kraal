//! Runbooks in the server: the picker, the target and parameters, the plan, runs server by
//! server with a log, and Markdown runbooks whose blocks run one at a time in a pane.

mod markdown;
mod prepare;
mod run;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use super::ui::{Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Server};
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::paths;
use crate::protocol::Placement;
use crate::runbooks::{self, Block, Body, On, Runbook, Source, Steps};
use crate::servers;

/// Where one copy of a runbook runs.
#[expect(
    clippy::large_enum_variant,
    reason = "a run has a handful of targets: a box around the server would only add an indirection"
)]
#[derive(Debug, Clone)]
pub enum Target {
    /// This machine.
    Local,
    /// Where a pane is (its server, or here in its directory).
    Pane(PaneId),
    Server(servers::Server),
}

/// Who asked for a run, and how to tell them how its start went.
#[derive(Clone, Copy)]
pub(in crate::server) struct Asker {
    /// The client whose workspace gets the run's tab (and who hears if it does not start).
    client: Option<ClientId>,
    /// A `kraal runbook run` waiting for the run's number as its answer.
    reply: Option<ClientId>,
}

/// A runbook being prepared: trust, target and parameters are asked one at a time, then the
/// plan.
pub struct Draft {
    /// Tells this draft from a later one of the same client (late background results).
    id: u32,
    runbook: Runbook,
    pub steps: Steps,
    /// The focused pane when it started: its context fills defaults and `on = "pane"`.
    pub pane: Option<PaneId>,
    pub targets: Option<Vec<Target>>,
    values: Vec<(String, String)>,
    /// The user approved this version of a repository runbook.
    trusted: bool,
    /// Choices of the parameter being asked, once its `choices_command` ran.
    choices: Option<Vec<String>>,
    /// Where the plan shown said it runs (each target's identity): it runs there or not at all.
    reviewed: Option<Vec<String>>,
}

impl Draft {
    fn new(runbook: Runbook, steps: Steps, pane: Option<PaneId>, targets: Option<Vec<Target>>) -> Draft {
        let trusted = runbooks::trusted(&runbook);
        Draft { id: 0, runbook, steps, pane, targets, values: Vec::new(), trusted, choices: None, reviewed: None }
    }

    /// `NAME=value` lines, secrets masked.
    fn shown_values(&self) -> Vec<String> {
        self.values
            .iter()
            .map(|(k, v)| {
                let param = self.steps.params.iter().find(|p| &p.name == k);
                format!("{k}={}", runbooks::shown_value(param, v))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    Waiting,
    Running,
    /// Over its timeout: asked to stop, not stopped yet.
    Stopping(Instant),
    Ok,
    Failed(Option<i32>),
    TimedOut,
    Skipped,
}

impl ItemState {
    fn failed(self) -> bool {
        matches!(self, ItemState::Failed(_) | ItemState::TimedOut)
    }

    fn pending(self) -> bool {
        matches!(self, ItemState::Waiting | ItemState::Running | ItemState::Stopping(_))
    }
}

pub struct RunItem {
    pub label: String,
    pub pane: PaneId,
    pub state: ItemState,
    started: Option<Instant>,
    /// The process its command started as, held while the target counts: a timeout stops it,
    /// and what it started in its process group, to the last process, even after the command
    /// itself or its pane is gone, without a signal ever reaching a program that got its
    /// number since.
    process: Option<crate::pane::ProcessHold>,
    /// When a timeout came to SIGKILL.
    killed: Option<Instant>,
}

/// A timed-out target whose pane was closed before all of it had ended: it is stopped all the
/// same.
pub struct StrayGroup {
    process: crate::pane::ProcessHold,
    /// When stopping it began.
    since: Instant,
    killed: Option<Instant>,
}

/// A runbook running, one pane per target.
pub struct Run {
    pub id: u32,
    pub name: String,
    tab: u32,
    pub items: Vec<RunItem>,
    /// Targets running at the same time.
    pub rolling: usize,
    pub stop_on_failure: bool,
    timeout: Option<Duration>,
    pub log: PathBuf,
    /// A pane of the run was closed: start nothing more.
    aborted: bool,
}

/// A Markdown runbook open for a client: its blocks run in `target`.
pub struct MdSession {
    runbook: Runbook,
    pub title: String,
    blocks: Vec<Block>,
    pub target: PaneId,
    /// Blocks sent to the pane.
    sent: HashSet<usize>,
    /// The production reason the user agreed to run steps under, for `target`.
    approved: Option<crate::server::contexts::Production>,
    /// The user agreed to type steps into a pane on a server.
    remote_ok: bool,
}

/// How long a reviewed plan can be run.
pub(super) const PLAN_LIFETIME: Duration = Duration::from_secs(600);

pub(super) fn first_line(code: &str) -> String {
    let first: String = code.lines().next().unwrap_or("").trim().chars().take(80).collect();
    if code.trim().lines().count() > 1 { format!("{first} …") } else { first }
}

/// Output lines of a `choices_command`, run where local steps would run.
pub(super) fn list_choices(cmd: &str, cwd: Option<&Path>, env: &[(String, String)]) -> Result<Vec<String>> {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd).envs(env.iter().cloned());
    // Where the steps would run, or not at all: never somewhere else.
    if let Some(d) = cwd {
        if !d.is_dir() {
            bail!("{} does not exist any more: cannot list the choices there", d.display());
        }
        c.current_dir(d);
    }
    let out = crate::exec::run(&mut c, Duration::from_secs(5), 256 << 10)?;
    if out.timed_out {
        bail!("{cmd:?} took more than 5 s");
    }
    if !out.success() {
        bail!("{cmd:?} failed: {}", out.stderr_text().trim());
    }
    Ok(out.stdout_text().lines().map(str::trim).filter(|l| !l.is_empty()).take(100).map(String::from).collect())
}

impl Server {
    // ---- picking -------------------------------------------------------------------------

    /// How to open the runbooks with the keys in use (`ctrl+b R b` by default).
    pub(super) fn runbooks_key(&self) -> String {
        self.keymap.hint(crate::keymap::Action::Runbooks).unwrap_or_else(|| "the Runbooks command".into())
    }

    /// The Runbooks command: the steps of the Markdown runbook in progress, or the list of
    /// runbooks.
    pub(super) fn runbooks_action(&mut self, client: ClientId) -> Mode {
        if let Some(s) = self.md_sessions.get(&client)
            && self.panes.contains_key(&s.target)
            && s.sent.len() < s.blocks.len()
        {
            let next = (0..s.blocks.len()).find(|i| !s.sent.contains(i)).unwrap_or(0);
            return Mode::Picker(self.md_picker(client, next, true));
        }
        self.runbook_list(client)
    }

    /// Show a picker that waits while `work` runs off the event loop, then the mode `then` makes
    /// of the answer if the client still waits for it. Keys typed meanwhile stay in the waiting
    /// picker (a search typed there carries over to a picker of the same kind); none reaches a
    /// pane. Esc gives up on the answer.
    pub(super) fn wait_for<T: Send + 'static>(
        &mut self,
        client: ClientId,
        kind: PickerKind,
        title: &str,
        work: impl FnOnce() -> T + Send + 'static,
        then: impl FnOnce(&mut Server, T) -> Mode + 'static,
    ) -> Mode {
        let id = self.alloc_id();
        let waiting = move |server: &Server| {
            server.clients.get(&client).is_some_and(|c| matches!(&c.mode, Mode::Picker(p) if p.loading == Some(id)))
        };
        let started = self.background(work, move |server, answer| {
            let Some(Mode::Picker(asked)) = server.clients.get(&client).map(|c| &c.mode).filter(|_| waiting(server))
            else {
                return;
            };
            let typed = (asked.query.clone(), asked.cursor);
            let mut next = then(server, answer);
            if let Mode::Picker(p) = &mut next
                && p.kind == kind
                && !typed.0.is_empty()
            {
                (p.query, p.cursor) = typed;
                p.selected = 0;
                p.refilter();
            }
            // `then` may have moved the client on by itself.
            if waiting(server)
                && let Some(c) = server.clients.get_mut(&client)
            {
                c.mode = next;
                c.dirty = true;
            }
            server.mark_dirty_client(client);
        });
        if !started {
            self.toast(client, "kraal could not start reading: try again");
            return Mode::Normal;
        }
        let mut picker = Picker::new(kind, title, Vec::new());
        picker.loading = Some(id);
        Mode::Picker(picker)
    }

    /// The list of runbooks, read off the event loop (files on a slow disk, or a repository's,
    /// which may be anything).
    pub(super) fn runbook_list(&mut self, client: ClientId) -> Mode {
        let focus = self.client_focus(client);
        let repo = focus.and_then(|p| self.pane_git(p)).map(|g| g.root.clone());
        self.wait_for(
            client,
            PickerKind::Runbook,
            "Runbooks",
            move || runbooks::all(repo.as_deref()),
            move |server, (list, errors)| server.runbook_picker(client, &list, &errors),
        )
    }

    fn runbook_picker(&mut self, client: ClientId, list: &[Runbook], errors: &[String]) -> Mode {
        if let Some(e) = errors.first() {
            self.toast(client, e.clone());
        }
        if list.is_empty() {
            if errors.is_empty() {
                let dir = paths::shorten_home(&runbooks::user_dir());
                self.toast(client, format!("no runbooks yet: `kraal runbook new <name>` creates one in {dir}"));
            }
            return Mode::Normal;
        }
        let items = list
            .iter()
            .map(|r| {
                let mut detail = vec![r.description()];
                match &r.body {
                    Body::Markdown { .. } => detail.push("document".into()),
                    Body::Steps(s) if s.on == On::Server => detail.push("on servers".into()),
                    Body::Steps(s) if s.on == On::Pane => detail.push("where the pane is".into()),
                    Body::Steps(_) => {}
                }
                if let Source::Repo(root) = &r.source {
                    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    detail.push(format!("repository {name}"));
                }
                detail.retain(|d| !d.is_empty());
                PickItem::new(r.name.clone(), detail.join(" · "), PickAction::Runbook(r.path.clone()))
            })
            .collect();
        Mode::Picker(Picker::new(PickerKind::Runbook, "Runbooks", items))
    }

    /// Open the runbook chosen in the list: read off the event loop too.
    pub(super) fn pick_runbook(&mut self, client: ClientId, path: &Path) -> Mode {
        let name = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let path = path.to_path_buf();
        self.wait_for(
            client,
            PickerKind::Runbook,
            &format!("Opening {name}"),
            move || runbooks::load(&path, runbooks::source_of(&path)),
            move |server, loaded| match loaded {
                Ok(r) => server.open_runbook(client, r),
                Err(e) => {
                    server.toast(client, format!("{e:#}"));
                    Mode::Normal
                }
            },
        )
    }

    fn open_runbook(&mut self, client: ClientId, r: Runbook) -> Mode {
        let pane = self.client_focus(client);
        match &r.body {
            Body::Steps(s) => {
                let steps = (**s).clone();
                let mut draft = Draft::new(r, steps, pane, None);
                draft.id = self.alloc_id();
                self.drafts.insert(client, draft);
                self.advance_draft(client)
            }
            Body::Markdown { .. } => self.open_markdown(client, r),
        }
    }

    /// Edit a runbook file in a floating pane.
    pub(super) fn edit_runbook(&mut self, client: ClientId, path: &Path) {
        let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return };
        let spec =
            PaneSpec { command: crate::edit::editor_argv(path, None), close_on_exit: true, ..Default::default() };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Floating, name) {
            self.toast(client, format!("{e:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn choices_are_listed_where_asked_or_not_at_all() {
        let gone = std::env::temp_dir().join(format!("kraal-choices-gone-{}", std::process::id()));
        let refused = super::list_choices("pwd", Some(&gone), &[]).unwrap_err().to_string();
        assert!(refused.contains("does not exist"), "{refused}");
        let here = std::env::temp_dir().canonicalize().unwrap();
        let listed = super::list_choices("pwd -P", Some(&here), &[]).unwrap();
        assert_eq!(listed, vec![here.display().to_string()]);
    }
}
