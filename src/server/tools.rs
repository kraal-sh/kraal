//! The tools picker, and starting a tool where the focused pane is.

use anyhow::{Context, Result, bail};

use super::contexts::Production;
use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Server};
use crate::layout::PaneId;
use crate::theme::NoticeKind;
use crate::tools::ToolDef;

impl Server {
    /// The tools that can run from `pane`, those it lacks something for dimmed.
    pub(super) fn tool_picker(&self, pane: Option<PaneId>) -> Picker {
        let ctx = pane.map(|p| self.tool_context(p)).unwrap_or_default();
        let mut items = Vec::new();
        for (name, t) in self.config.tools.iter().filter(|(_, t)| t.enabled && t.installed(&ctx)) {
            let mut detail = t.description.clone();
            if t.runs_remote(&ctx)
                && let Some(s) = &ctx.server
            {
                detail.push_str(&format!(" · on {}", s.name));
            }
            if let Some(keys) = self.keymap.leader_hint(&crate::keymap::Command::Tool(name.clone())) {
                detail.push_str(&format!(" · {keys}"));
            }
            let missing = t.missing(&ctx);
            if let Some(why) = missing {
                detail.push_str(&format!(" · {why}"));
            }
            items.push(PickItem {
                label: name.clone(),
                detail: detail.trim_start_matches(" · ").to_string(),
                action: PickAction::Tool { pane, name: name.clone() },
                state: None,
                dim: missing.is_some(),
            });
        }
        // Tools that can run first.
        items.sort_by_key(|i| i.dim);
        let prod = ctx.production.as_ref().map(|why| format!("PRODUCTION ({why}) · ")).unwrap_or_default();
        let place = crate::text::truncate_start(&ctx.place(), 40);
        Picker::new(PickerKind::Tool, &format!("Tools · {prod}{place}"), items)
    }

    /// Start tool `name` for `pane`, asking first when the pane is production and the tool is not
    /// marked `read_only`.
    pub(super) fn tool_request(
        &mut self,
        client: ClientId,
        pane: Option<PaneId>,
        name: &str,
        selection: Option<String>,
    ) -> Mode {
        let place = pane.map(|p| self.tool_place(p));
        self.tool_request_after(client, pane, name, selection, None, place)
    }

    /// Where a tool asked for in `pane` runs: its directory here, or its server and directory
    /// there, with the kubeconfig and flags its commands use, or the contexts its shell there
    /// reported. A tool runs where it was asked for or not at all: the pane may move while the
    /// tool is checked, or while its question waits for an answer.
    pub(super) fn tool_place(&self, pane: PaneId) -> String {
        let ctx = self.tool_context(pane);
        let flags = self.panes.get(&pane).map(|p| p.seen.kube_flags.clone());
        let reported = self.pane_reported(pane);
        format!("{:?}", (ctx.cwd, ctx.remote_cwd, ctx.server, ctx.kubeconfig, flags, reported))
    }

    /// [`Server::tool_request`], `accepted` being the production reason the user already said yes
    /// to, and `place` where the tool was asked for. A tool that acts goes by what the pane's
    /// kubeconfig says now, not at the last scan (`kubectx` may have just switched it to
    /// production): it is read again first, off the event loop, while the client waits in a
    /// list that keeps its keys (Esc gives up, and nothing starts). A file that does not answer
    /// in time is an unknown context.
    pub(super) fn tool_request_after(
        &mut self,
        client: ClientId,
        pane: Option<PaneId>,
        name: &str,
        selection: Option<String>,
        accepted: Option<Production>,
        place: Option<String>,
    ) -> Mode {
        let read_only = self.config.tools.get(name).is_some_and(|t| t.read_only);
        if !read_only
            && let Some(p) = pane
            && let Some(value) = self.kubeconfig_of(p)
        {
            let (cache, read, tool) = (self.kube_cache.clone(), value.clone(), name.to_string());
            return self.wait_for(
                client,
                PickerKind::Tool,
                &format!("{name} · reading the Kubernetes context"),
                move || cache.read_within(&read, super::contexts::CONTEXT_STALE),
                move |server, fresh| {
                    let stalled = match fresh {
                        Some((number, file)) => {
                            server.tell_kube(value, number, file);
                            None
                        }
                        None => Some(server.kube_stalled(p, &value)),
                    };
                    server.tool_decide(client, pane, &tool, selection, accepted, stalled, place)
                },
            );
        }
        self.tool_decide(client, pane, name, selection, accepted, None, place)
    }

    /// Run the tool, or ask first when the pane is production for a reason not accepted yet.
    /// `stalled` is the pane's kubeconfig not answering the read made for this tool.
    #[expect(clippy::too_many_arguments, reason = "the tool's request and what was decided about it so far")]
    fn tool_decide(
        &mut self,
        client: ClientId,
        pane: Option<PaneId>,
        name: &str,
        selection: Option<String>,
        accepted: Option<Production>,
        stalled: Option<Production>,
        place: Option<String>,
    ) -> Mode {
        if let Some(p) = pane {
            if !self.panes.contains_key(&p) {
                self.toast(client, NoticeKind::Warning, format!("{name}: the pane it was for is gone"));
                return Mode::Normal;
            }
            if place.as_ref().is_some_and(|asked| *asked != self.tool_place(p)) {
                let (label, now) = (self.pane_label(p), self.tool_context(p).place());
                self.toast(
                    client,
                    NoticeKind::Warning,
                    format!("{name} was not started: {label} moved since it was asked for (now {now})"),
                );
                return Mode::Normal;
            }
        }
        let read_only = self.config.tools.get(name).is_some_and(|t| t.read_only);
        let why = match (pane.and_then(|p| self.pane_production(p)), stalled) {
            (Some(known), Some(stalled)) => Some(known.and(stalled)),
            (known, stalled) => known.or(stalled),
        };
        if !read_only
            && let Some(why) = why
            && accepted.as_ref() != Some(&why)
        {
            let label = pane.map(|p| self.pane_label(p)).unwrap_or_default();
            let message = format!(
                "Run {name} where {label} is? It is production ({why}).\n\
                 Tools that only look can say read_only = true in the config to skip this question."
            );
            let action = ConfirmAction::RunTool { pane, name: name.into(), selection, reason: why, place };
            return Mode::confirm(message, action);
        }
        if let Err(e) = self.run_tool(client, pane, name, selection) {
            self.toast(client, NoticeKind::Error, format!("{e:#}"));
        }
        Mode::Normal
    }

    /// Start tool `name` for `pane`: on its server, or here in its directory. `selection`
    /// replaces the pane's selected text (a hint's match).
    pub(super) fn run_tool(
        &mut self,
        client: ClientId,
        pane: Option<PaneId>,
        name: &str,
        selection: Option<String>,
    ) -> Result<PaneId> {
        let tool = self
            .config
            .tools
            .get(name)
            .filter(|t| t.enabled)
            .with_context(|| format!("no tool {name:?} in the config"))?
            .clone();
        // A tool asked for a pane runs where that pane is, or not at all: a pane closed since
        // (by another client, say) must not turn into "here".
        let mut ctx = match pane {
            Some(p) if !self.panes.contains_key(&p) => bail!("{name}: the pane it was for is gone"),
            Some(p) => self.tool_context(p),
            None => Default::default(),
        };
        if selection.is_some() {
            ctx.selection = selection;
        }
        if let Some(why) = tool.missing(&ctx) {
            bail!("{name}: {why}");
        }
        if !tool.installed(&ctx) {
            bail!("{name}: {} is not installed", tool.program().as_deref().unwrap_or(name));
        }
        let spec = tool.spec(&ctx, &|s, remote| self.ssh_command_running(s, remote));
        let ws = self.clients.get(&client).map(|c| c.workspace).context("no client")?;
        let label = match &ctx.server {
            Some(s) if tool.runs_remote(&ctx) => format!("{name} · {}", s.name),
            _ => name.to_string(),
        };
        ToolDef::check_cwd(&spec)?;
        self.spawn_pane_in(ws, pane, spec, tool.open.placement(), Some(label))
    }
}
