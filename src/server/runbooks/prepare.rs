//! Preparing a step runbook: trust, target, parameters, then the plan to approve (from the
//! picker, or from `kraal runbook run`).

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use super::{Draft, PLAN_LIFETIME, Target, first_line, list_choices};
use crate::layout::PaneId;
use crate::runbooks::{self, Body, On, Source};
use crate::server::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind, Prompt, PromptAction};
use crate::server::{ClientId, Server};
use crate::{paths, servers};

impl Server {
    /// Ask for what the draft still needs, in order: trust (a repository runbook, before any of
    /// its commands runs), target, parameters; then show the plan.
    pub(in crate::server) fn advance_draft(&mut self, client: ClientId) -> Mode {
        let Some(d) = self.drafts.get(&client) else { return Mode::Normal };
        if !d.trusted {
            return Mode::confirm(self.trust_question(d), ConfirmAction::TrustRunbook);
        }
        if d.targets.is_none() {
            let targets = match d.steps.on {
                On::Local => vec![Target::Local],
                On::Pane => vec![d.pane.map(Target::Pane).unwrap_or(Target::Local)],
                On::Server => return Mode::Picker(self.target_picker(d.pane, &d.runbook.name)),
            };
            self.drafts.get_mut(&client).unwrap().targets = Some(targets);
        }
        let d = &self.drafts[&client];
        let Some(param) = d.steps.params.get(d.values.len()).cloned() else {
            let message = self.plan(d, false);
            let reviewed = self.identities(d);
            if let Some(d) = self.drafts.get_mut(&client) {
                d.reviewed = Some(reviewed);
            }
            return Mode::confirm(message, ConfirmAction::RunRunbook);
        };
        match (&param.choices_command, &d.choices) {
            // List the choices in the background: the command may be slow.
            (Some(cmd), None) if param.choices.is_empty() => {
                let (cmd, cwd, vars) = (cmd.clone(), self.draft_cwd(d), self.draft_vars(d));
                let (draft, asked) = (d.id, d.values.len());
                self.toast(client, format!("listing choices for {}…", param.name));
                self.background(
                    move || list_choices(&cmd, cwd.as_deref(), &vars),
                    move |server, listed| {
                        // Only for the question that asked: not a newer draft, not a later one.
                        let current =
                            server.drafts.get(&client).is_some_and(|d| d.id == draft && d.values.len() == asked);
                        if !current {
                            return;
                        }
                        let choices = listed.unwrap_or_else(|e| {
                            // Without its list, the parameter is asked as text.
                            server.toast(client, format!("{e:#}"));
                            Vec::new()
                        });
                        if let Some(d) = server.drafts.get_mut(&client) {
                            d.choices = Some(choices);
                        }
                        let next = server.advance_draft(client);
                        server.set_mode_if_idle(client, next);
                    },
                );
                Mode::Normal
            }
            _ => self.param_question(client),
        }
    }

    pub(super) fn trust_question(&self, d: &Draft) -> String {
        let root = match &d.runbook.source {
            Source::Repo(root) => paths::shorten_home(root),
            Source::User => String::new(),
        };
        let mut lines = vec![
            format!("Trust the runbook {} from the repository {root}?", d.runbook.name),
            "It is new, or changed since you approved it. It runs:".into(),
        ];
        let steps: Vec<&String> = d.steps.steps.iter().filter(|s| !s.trim().is_empty()).collect();
        for (i, st) in steps.iter().enumerate().take(8) {
            lines.push(format!("  {}. {}", i + 1, first_line(st)));
        }
        if steps.len() > 8 {
            let key = self.runbooks_key();
            lines.push(format!("  … and {} more steps ({key}, ctrl+e shows the file)", steps.len() - 8));
        }
        for p in d.steps.params.iter().filter_map(|p| p.choices_command.as_ref()) {
            lines.push(format!("  and lists choices with: {}", first_line(p)));
        }
        lines.join("\n")
    }

    pub(in crate::server) fn confirm_trust(&mut self, client: ClientId, yes: bool) -> Mode {
        if !yes {
            self.drafts.remove(&client);
            return Mode::Normal;
        }
        let Some(d) = self.drafts.get_mut(&client) else { return Mode::Normal };
        if let Err(e) = runbooks::trust(&d.runbook) {
            self.warnings.push(format!("{e:#}"));
        }
        d.trusted = true;
        self.advance_draft(client)
    }

    /// The question for the parameter being asked: a list of choices, or a prompt.
    pub(super) fn param_question(&self, client: ClientId) -> Mode {
        let Some(d) = self.drafts.get(&client) else { return Mode::Normal };
        let Some(p) = d.steps.params.get(d.values.len()) else { return Mode::Normal };
        let vars = self.draft_vars(d);
        let default = p.default.as_deref().map(|v| runbooks::expand_vars(v, &vars)).unwrap_or_default();
        let choices = if p.choices.is_empty() { d.choices.clone().unwrap_or_default() } else { p.choices.clone() };
        if choices.is_empty() {
            // A secret's default is in the field too, masked: Enter keeps it, as on the command
            // line; the title says there is one, since it cannot be read.
            let title = if p.secret && !default.is_empty() {
                format!("{} (Enter keeps the default)", p.prompt())
            } else {
                p.prompt()
            };
            let mut prompt = Prompt::new(&title, &default, PromptAction::RunbookValue);
            prompt.masked = p.secret;
            return Mode::Prompt(prompt);
        }
        let items: Vec<PickItem> = choices
            .into_iter()
            .map(|c| {
                let detail = if c == default { "default" } else { "" };
                PickItem::new(c.clone(), detail, PickAction::RunbookValue(c))
            })
            .collect();
        let mut picker = Picker::new(PickerKind::RunbookParam, &p.prompt(), items);
        if let Some(i) = picker.filtered.iter().position(|&i| picker.items[i].label == default) {
            picker.selected = i;
        }
        Mode::Picker(picker)
    }

    /// Servers and groups to run on, the focused pane's server first.
    pub(super) fn target_picker(&self, pane: Option<PaneId>, name: &str) -> Picker {
        let words = &self.config.production.words;
        let mut items = Vec::new();
        if let Some(s) = pane.and_then(|p| self.pane_server_entry(p)) {
            items.push(PickItem::new(s.name.clone(), "this pane's server", PickAction::RunbookTarget(s.name.clone())));
        }
        for (g, n) in servers::groups(&self.known_servers) {
            let detail = format!("group · {n} server{}", if n == 1 { "" } else { "s" });
            items.push(PickItem::new(format!("@{g}"), detail, PickAction::RunbookTarget(format!("@{g}"))));
        }
        for k in &self.known_servers {
            if items.iter().any(|i| i.label == k.server.name) {
                continue;
            }
            let prod = if crate::context::server_production(&k.server, words) { "PROD · " } else { "" };
            let detail = format!("{prod}{}", k.server.address());
            items.push(PickItem::new(k.server.name.clone(), detail, PickAction::RunbookTarget(k.server.name.clone())));
        }
        Picker::new(PickerKind::RunbookTarget, &format!("Run {name} on…"), items)
    }

    /// `web01`, `user@host` or `@group` as targets.
    pub(in crate::server) fn resolve_targets(&self, spec: &str) -> Result<Vec<Target>> {
        if let Some(group) = spec.strip_prefix('@') {
            let members: Vec<Target> = self
                .known_servers
                .iter()
                .filter(|k| k.server.groups.iter().any(|g| g == group))
                .map(|k| Target::Server(k.server.clone()))
                .collect();
            if members.is_empty() {
                bail!("no server in group {group:?}");
            }
            return Ok(members);
        }
        let s = self.resolve_server(spec).with_context(|| format!("not a server: {spec}"))?;
        Ok(vec![Target::Server(s)])
    }

    pub(in crate::server) fn set_draft_target(&mut self, client: ClientId, spec: &str) -> Mode {
        match self.resolve_targets(spec) {
            Ok(t) => {
                if let Some(d) = self.drafts.get_mut(&client) {
                    d.targets = Some(t);
                }
                self.advance_draft(client)
            }
            Err(e) => {
                self.toast(client, format!("{e:#}"));
                Mode::Normal
            }
        }
    }

    pub(in crate::server) fn set_draft_value(&mut self, client: ClientId, value: String) -> Mode {
        if let Some(d) = self.drafts.get_mut(&client)
            && let Some(p) = d.steps.params.get(d.values.len())
        {
            let name = p.name.clone();
            d.values.push((name, value));
            d.choices = None;
        }
        self.advance_draft(client)
    }

    /// The focused pane's variables, for parameter defaults and choice commands.
    pub(super) fn draft_vars(&self, d: &Draft) -> Vec<(String, String)> {
        d.pane.map(|p| self.tool_context(p).variables(false)).unwrap_or_default()
    }

    /// Directory of local runs: the runbook's `cwd`, the repository, or the pane's directory.
    pub(super) fn draft_cwd(&self, d: &Draft) -> Option<PathBuf> {
        let pane_cwd =
            d.pane.and_then(|p| self.panes.get(&p)).filter(|p| p.ssh.is_none()).and_then(|p| p.current_cwd());
        match (&d.steps.cwd, &d.runbook.source) {
            (Some(c), Source::Repo(root)) => Some(root.join(paths::expand_tilde(c))),
            (Some(c), Source::User) => {
                let p = paths::expand_tilde(c);
                Some(if p.is_relative() { pane_cwd.unwrap_or_default().join(p) } else { p })
            }
            (None, Source::Repo(root)) => Some(root.clone()),
            (None, Source::User) => pane_cwd,
        }
    }

    pub(super) fn target_label(&self, t: &Target) -> String {
        match t {
            Target::Local => "this machine".into(),
            Target::Pane(p) => match self.pane_server(*p) {
                Some(s) => s.name,
                None => self.pane_label(*p),
            },
            Target::Server(s) => s.name.clone(),
        }
    }

    pub(super) fn target_production(
        &self,
        t: &Target,
        pane: Option<PaneId>,
    ) -> Option<crate::server::contexts::Production> {
        match t {
            // Local runs carry the pane's Kubernetes, Terraform and AWS context.
            Target::Local => pane.filter(|p| self.pane_server(*p).is_none()).and_then(|p| self.pane_production(p)),
            Target::Pane(p) => self.pane_production(*p),
            Target::Server(s) => {
                let entry = self.known_servers.iter().find(|k| k.server.name == s.name).map(|k| &k.server).unwrap_or(s);
                crate::context::server_production(entry, &self.config.production.words).then(|| {
                    crate::server::contexts::Production::new(format!("server {}", s.name), format!("server {entry:?}"))
                })
            }
        }
    }

    /// What will run, where and how. `full` lists every line of every step (the CLI); otherwise
    /// the first line of the first steps (the confirmation box).
    pub(in crate::server) fn plan(&self, d: &Draft, full: bool) -> String {
        let s = &d.steps;
        let targets = d.targets.as_deref().unwrap_or(&[]);
        let names: Vec<String> = targets.iter().map(|t| self.target_label(t)).collect();
        let n = names.len();
        let place = if n == 1 {
            names[0].clone()
        } else {
            let mut list = names.iter().take(if full { n } else { 4 }).cloned().collect::<Vec<_>>().join(", ");
            if !full && n > 4 {
                list.push_str(&format!(" and {} more", n - 4));
            }
            format!("{n} servers ({list})")
        };
        let mut how = String::new();
        if n > 1 {
            how = match s.rolling {
                0 => ", all at once".into(),
                1 => ", one at a time".into(),
                k => format!(", {k} at a time"),
            };
            if s.stop_on_failure {
                how.push_str(", stopping at the first failure");
            }
        }
        if let Some(t) = &s.timeout {
            how.push_str(&format!(", at most {t} each"));
        }
        let mut lines = vec![format!("Run {} on {place}{how}?", d.runbook.name)];
        let prod: Vec<String> = targets
            .iter()
            .filter(|t| self.target_production(t, d.pane).is_some())
            .map(|t| self.target_label(t))
            .collect();
        if !prod.is_empty() {
            lines.push(format!("PRODUCTION: {}", prod.join(", ")));
        }
        if let Source::Repo(root) = &d.runbook.source
            && !d.trusted
        {
            lines.push(format!(
                "From the repository {}: new, or changed since you approved it. It runs once this version is approved.",
                paths::shorten_home(root)
            ));
        }
        lines.extend(d.shown_values());
        let steps: Vec<&String> = s.steps.iter().filter(|st| !st.trim().is_empty()).collect();
        for (i, st) in steps.iter().enumerate() {
            if !full && i == 8 {
                lines.push(format!("… and {} more steps", steps.len() - 8));
                break;
            }
            if full {
                for (j, line) in st.lines().enumerate() {
                    lines.push(if j == 0 { format!("{}. {line}", i + 1) } else { format!("   {line}") });
                }
            } else {
                lines.push(format!("{}. {}", i + 1, first_line(st)));
            }
        }
        lines.join("\n")
    }

    /// What identifies each target of a draft: its name, where it runs (server, directory) and
    /// its production reason.
    pub(super) fn identities(&self, d: &Draft) -> Vec<String> {
        d.targets
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|t| {
                let place = match t {
                    Target::Local => self.draft_cwd(d).map(|c| c.display().to_string()).unwrap_or_default(),
                    Target::Pane(p) => self.tool_context(*p).place(),
                    Target::Server(s) => s.address(),
                };
                format!("{} @ {place} · {:?}", self.target_label(t), self.target_production(t, d.pane))
            })
            .collect()
    }

    pub(in crate::server) fn confirm_runbook(&mut self, client: ClientId, yes: bool) {
        let Some(d) = self.drafts.remove(&client) else { return };
        if !yes {
            self.toast(client, "runbook cancelled");
            return;
        }
        if let Err(e) = self.launch_runbook(d, super::Asker { client: Some(client), reply: None }) {
            self.toast(client, format!("{e:#}"));
        }
    }

    /// `kraal runbook run`: resolve everything now (file, targets, parameters) and keep it, with
    /// where each target is; return the plan's text and a token that runs it, or refuses if a
    /// target is no longer where the plan said.
    pub(in crate::server) fn prepare_runbook(
        &mut self,
        path: &Path,
        target: Option<&str>,
        params: Vec<(String, String)>,
        pane: Option<PaneId>,
    ) -> Result<(String, String)> {
        let r = runbooks::load(path, runbooks::source_of(path))?;
        let Body::Steps(steps) = &r.body else {
            bail!("{} is a document: open it in kraal with {} and run its steps there", r.name, self.runbooks_key())
        };
        let steps = (**steps).clone();
        let targets = match (target, steps.on) {
            (Some(t), _) => self.resolve_targets(t)?,
            (None, On::Server) => bail!("{} runs on servers: give --on SERVER or --on @group", r.name),
            (None, On::Pane) => vec![pane.map(Target::Pane).unwrap_or(Target::Local)],
            (None, On::Local) => vec![Target::Local],
        };
        for (k, _) in &params {
            if !steps.params.iter().any(|p| &p.name == k) {
                bail!("{} has no parameter {k}", r.name);
            }
        }
        let mut d = Draft::new(r, steps, pane, Some(targets));
        let vars = self.draft_vars(&d);
        let mut missing = Vec::new();
        for p in &d.steps.params {
            match params.iter().find(|(k, _)| k == &p.name) {
                Some((_, v)) => d.values.push((p.name.clone(), v.clone())),
                None => match &p.default {
                    Some(def) => d.values.push((p.name.clone(), runbooks::expand_vars(def, &vars))),
                    None => missing.push(p.name.clone()),
                },
            }
        }
        if !missing.is_empty() {
            bail!("missing parameters: {} (-p NAME=value)", missing.join(", "));
        }
        let text = self.plan(&d, true);
        d.reviewed = Some(self.identities(&d));
        let token = crate::accounts::new_uuid();
        self.plans.retain(|_, (made, _)| made.elapsed() < PLAN_LIFETIME);
        self.plans.insert(token.clone(), (Instant::now(), d));
        Ok((text, token))
    }

    /// Run a plan prepared by [`Server::prepare_runbook`] for the CLI client `reply`, which gets
    /// the run's number once its targets have started (or why they did not). Asking for a plan
    /// to run approves nothing (whoever asks may have shown it to nobody): a repository
    /// runbook runs if the version planned is approved by now.
    pub(in crate::server) fn run_prepared(&mut self, token: &str, reply: ClientId) -> Result<()> {
        let (made, mut d) = self.plans.remove(token).context("no such plan: review the runbook again")?;
        if made.elapsed() > PLAN_LIFETIME {
            bail!("the plan is more than 10 minutes old: review the runbook again");
        }
        d.trusted = runbooks::trusted(&d.runbook);
        self.launch_runbook(d, super::Asker { client: None, reply: Some(reply) })
    }
}
