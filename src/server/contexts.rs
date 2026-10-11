//! What each pane works on besides git (Kubernetes context, Terraform workspace, AWS profile),
//! whether it is production, and the context handed to tools.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::Server;
use crate::config::ContextsConfig;
use crate::context::{self, KubeContext, TfSeen};
use crate::layout::PaneId;
use crate::pane::Pane;
use crate::tools::ToolContext;

/// A context file read for longer than this does not answer (a stalled network disk): what it
/// says is unknown, which the production guard treats as possibly production.
pub(super) const CONTEXT_STALE: Duration = Duration::from_secs(5);

/// Why a pane may be production, and exactly what was seen there. Two are the same approval
/// only when what they are about is the same: the same connection, the same kubeconfig with the
/// same flags as read then, the same Terraform directory and workspace; not merely the same
/// words (two kubeconfigs may both name a context `prod`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Production {
    /// What the user is told: `Kubernetes context prod-eks`.
    pub why: String,
    /// What it is about, compared but not shown.
    what: String,
}

impl Production {
    pub fn new(why: impl Into<String>, what: impl Into<String>) -> Production {
        Production { why: why.into(), what: what.into() }
    }

    /// Both reasons, for a pane that may be production twice over.
    pub fn and(self, other: Production) -> Production {
        if self == other {
            return self;
        }
        Production { why: format!("{}; {}", self.why, other.why), what: format!("{} + {}", self.what, other.what) }
    }
}

impl std::fmt::Display for Production {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

/// What a Kubernetes context is about, as an approval compares it: the `KUBECONFIG` value, the
/// flags the pane's tools gave (`--context`, `--namespace`), and the read since which what is
/// known of it holds (a newer content is a newer read).
fn kube_what(value: &str, flags: &context::KubeFlags, read: u64) -> String {
    format!("kubeconfig {value:?} {flags:?} read #{read}")
}

/// A directory a Terraform run worked on, with its data directory.
type TfDir = (PathBuf, Option<String>);

/// The Terraform workspaces of the directories panes are in and of those Terraform runs worked
/// on, read off the event loop, each read on its own: a slow `git status` or another directory
/// that does not answer never makes one look stalled.
/// Directories read for a Terraform workspace kept once no pane is in them, before those that
/// are not Terraform directories are forgotten.
const TF_DIRS_KEPT: usize = 512;

/// Past [`TF_DIRS_KEPT`] entries, forget the directories no pane wants that are not Terraform
/// directories.
fn forget_unused(read: &mut HashMap<TfDir, Option<String>>, wanted: &HashSet<TfDir>) {
    if read.len() > TF_DIRS_KEPT {
        read.retain(|k, ws| wanted.contains(k) || ws.is_some());
    }
}

#[derive(Default)]
pub(super) struct TfDirs {
    /// What each has selected (`None`: it is not a Terraform directory).
    read: HashMap<TfDir, Option<String>>,
    /// Being read: since when, and the read's number (see `KubeCache`).
    reading: HashMap<TfDir, (Instant, u64)>,
    reads: u64,
}

/// A Terraform directory as a reason names it, with its data directory.
fn tf_place((dir, data_dir): &TfDir) -> String {
    match data_dir {
        Some(d) => format!("{}, TF_DATA_DIR {d}", dir.display()),
        None => dir.display().to_string(),
    }
}

/// Remember what the Kubernetes, AWS and Terraform commands running in a local pane work on:
/// their flags first, then their environment. Nothing is read here but `/proc`; `true` when
/// there are context files to read again (off the event loop).
pub(super) fn observe_tools(cfg: &ContextsConfig, pane: &mut Pane, argv: &[String]) -> bool {
    let kube = cfg.kubernetes && context::runs_kube_tool(argv);
    let aws = cfg.aws && context::runs_aws_tool(argv);
    let tf = cfg.terraform && context::runs_terraform(argv);
    if !kube && !aws && !tf {
        return false;
    }
    let Some(pid) = pane.foreground_pid() else { return false };
    let before = (pane.seen.kubeconfig.clone(), pane.seen.tf_workspace.clone());
    let env = crate::process::environ(
        pid,
        &["KUBECONFIG", "AWS_PROFILE", "AWS_DEFAULT_PROFILE", "TF_WORKSPACE", "TF_DATA_DIR"],
    );
    if kube {
        let flags = context::kube_flags(argv);
        let value = flags.kubeconfig.clone().or_else(|| env.get("KUBECONFIG").cloned()).unwrap_or_default();
        let base = crate::process::cwd(pid);
        pane.seen.kubeconfig = Some(context::absolute_kubeconfig(&value, base.as_deref()));
        pane.seen.kube_flags = context::KubeFlags { kubeconfig: None, ..flags };
    }
    if aws {
        let profile = context::aws_profile_flag(argv)
            .or_else(|| env.get("AWS_PROFILE").cloned())
            .or_else(|| env.get("AWS_DEFAULT_PROFILE").cloned())
            .filter(|p| !p.is_empty());
        pane.seen.aws_profile = Some(profile);
    }
    if tf {
        let cwd = crate::process::cwd(pid);
        pane.seen.tf_workspace = match &cwd {
            Some(dir) => Some(context::terraform_command_workspace(argv, dir, &env)),
            None => env.get("TF_WORKSPACE").filter(|w| !w.is_empty()).map(|w| TfSeen::Env(w.clone())),
        };
    }
    (pane.seen.kubeconfig.clone(), pane.seen.tf_workspace.clone()) != before
}

/// A variable the pane's spec sets (workspace files can give a pane its `KUBECONFIG`).
fn spec_env(pane: &Pane, name: &str) -> Option<String> {
    pane.spec.env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

impl Server {
    fn pane_kubeconfig(&self, pane: &Pane) -> Option<String> {
        if !self.config.contexts.kubernetes || pane.ssh.is_some() {
            return None;
        }
        // A workspace file's relative `KUBECONFIG` is relative to the pane's directory, as for
        // the pane's own commands: not to wherever the server runs.
        pane.seen.kubeconfig.clone().or_else(|| {
            spec_env(pane, "KUBECONFIG").map(|v| context::absolute_kubeconfig(&v, pane.spec.cwd.as_deref()))
        })
    }

    /// Re-read the current context of the kubeconfigs panes use (`kubectx` changes it). The
    /// files are read off the event loop, each `KUBECONFIG` value on its own: a slow disk, or a
    /// path that is not a file, must not stop the server, nor the other panes' contexts.
    pub(super) fn refresh_kube(&mut self) {
        let values: HashSet<String> = self.panes.values().filter_map(|p| self.pane_kubeconfig(p)).collect();
        self.kube.retain(|v, _| values.contains(v));
        self.kube_cache.retain_told(|v| values.contains(v));
        for value in values {
            let Some(number) = self.kube_cache.begin(&value) else { continue };
            let (cache, read, done) = (self.kube_cache.clone(), value.clone(), value.clone());
            let started = self.background(
                move || cache.merged(&read),
                move |server, file| {
                    server.kube_cache.end(&done);
                    server.tell_kube(done, number, file);
                },
            );
            if !started {
                self.kube_cache.end(&value);
            }
        }
    }

    /// What read `number` of a `KUBECONFIG` value found, unless a newer read told already.
    pub(super) fn tell_kube(&mut self, value: String, number: u64, file: context::KubeFile) {
        let changed = self.kube.get(&value) != Some(&file);
        if self.kube_cache.told(&value, number, changed) && changed {
            self.kube.insert(value, file);
            self.mark_dirty();
        }
    }

    /// The `KUBECONFIG` value (`""`: the default file) the tools of a pane act on.
    pub(super) fn kubeconfig_of(&self, id: PaneId) -> Option<String> {
        self.pane_kubeconfig(self.panes.get(&id)?)
    }

    /// How a pane's kubeconfig is named in a reason: with the flags its tools gave, so that an
    /// approval for one cluster or namespace is not taken for another.
    fn kube_place(&self, id: PaneId, value: &str) -> String {
        let mut place = if value.is_empty() { "~/.kube/config".to_string() } else { value.to_string() };
        if let Some(p) = self.panes.get(&id) {
            if let Some(c) = &p.seen.kube_flags.context {
                place.push_str(&format!(", context {c}"));
            }
            if let Some(ns) = &p.seen.kube_flags.namespace {
                place.push_str(&format!(", namespace {ns}"));
            }
        }
        place
    }

    /// What a pane's Kubernetes context is about, as an approval compares it (see
    /// [`kube_what`]).
    fn kube_what(&self, id: PaneId, value: &str, read: u64) -> String {
        let flags = self.panes.get(&id).map(|p| p.seen.kube_flags.clone()).unwrap_or_default();
        kube_what(value, &flags, read)
    }

    /// The reason a tool gives when the pane's kubeconfig did not answer the read made for it:
    /// about that file with those flags, as last known (another file, other flags or a newer
    /// read are not the same approval; the same stalled file asked about twice is).
    pub(super) fn kube_stalled(&self, id: PaneId, value: &str) -> Production {
        let read = self.kube_cache.known_since(value).unwrap_or(0);
        Production::new(
            format!("an unknown Kubernetes context ({} does not answer)", self.kube_place(id, value)),
            format!("stalled {}", self.kube_what(id, value, read)),
        )
    }

    pub fn pane_kube(&self, id: PaneId) -> Option<KubeContext> {
        let p = self.panes.get(&id)?;
        self.kube.get(&self.pane_kubeconfig(p)?)?.resolve(&p.seen.kube_flags)
    }

    /// Why the Kubernetes context of the pane's tools cannot be known: its kubeconfig exists
    /// but cannot be read, or does not answer. Unknown is not "no cluster": it may well be
    /// production. Each is about the file, the flags and the read it comes from: an approval
    /// given for one unknown context is not taken for another, nor for the same file once it
    /// changed.
    fn pane_kube_unknown(&self, id: PaneId) -> Option<Production> {
        if let Some(stalled) = self.pane_kube_stalled(id) {
            return Some(stalled);
        }
        let p = self.panes.get(&id)?;
        let value = self.pane_kubeconfig(p)?;
        let f = self.kube.get(&value)?;
        if p.seen.kube_flags.context.is_some() {
            return None;
        }
        let read = self.kube_cache.known_since(&value).unwrap_or(0);
        f.unreadable.as_ref().map(|why| {
            Production::new(
                format!("an unknown Kubernetes context (unreadable kubeconfig: {why})"),
                format!("unreadable {}", self.kube_what(id, &value, read)),
            )
        })
    }

    /// Why the pane's kubeconfig is unknown while it is read again and does not answer: what was
    /// read of it before may have changed, so it no longer counts, production or not (an approval
    /// given for it does not hold for the file being read).
    fn pane_kube_stalled(&self, id: PaneId) -> Option<Production> {
        let p = self.panes.get(&id)?;
        let value = self.pane_kubeconfig(p)?;
        let (since, number) = self.kube_cache.reading(&value)?;
        (since.elapsed() > CONTEXT_STALE).then(|| {
            Production::new(
                format!("an unknown Kubernetes context ({} does not answer)", self.kube_place(id, &value)),
                format!("stalled {}", self.kube_what(id, &value, number)),
            )
        })
    }

    /// Whether the Kubernetes context shown for the pane is unknown (`⎈ ?`).
    pub fn pane_kube_is_unknown(&self, id: PaneId) -> bool {
        self.pane_kube_unknown(id).is_some()
    }

    /// Why the pane's kubeconfig is not known yet: it was just seen and is being read. Its
    /// context is not known, which is not "no cluster" either.
    fn pane_kube_pending(&self, id: PaneId) -> Option<Production> {
        let value = self.kubeconfig_of(id)?;
        if self.kube.contains_key(&value) {
            return None;
        }
        let number = self.kube_cache.reading(&value).map_or(0, |(_, n)| n);
        Some(Production::new(
            format!("a Kubernetes context not read yet ({})", self.kube_place(id, &value)),
            format!("pending {}", self.kube_what(id, &value, number)),
        ))
    }

    /// Whether a context of the pane is being read again right now, and has not been for long
    /// (then it is unknown, see [`Server::pane_production`]): what is known of it may be about
    /// to change. Input broadcast to such a pane waits for the read (see `held_input`).
    pub fn pane_context_reading(&self, id: PaneId) -> bool {
        let fresh = |since: &Instant| since.elapsed() <= CONTEXT_STALE;
        let kube = self.kubeconfig_of(id).and_then(|v| self.kube_cache.reading(&v)).is_some_and(|(t, _)| fresh(&t));
        let tf = self.pane_tf_keys(id).any(|k| self.tf_dirs.reading.get(&k).is_some_and(|(t, _)| fresh(t)));
        kube || tf
    }

    /// Read again, off the event loop, the workspaces of the directories panes are in and of
    /// those Terraform runs worked on. What a run found holds while the pane stays in the
    /// directory it ran from: once it left, the run is forgotten (the directory's own workspace,
    /// still remembered, counts when it comes back).
    pub(super) fn refresh_tf_dirs(&mut self) {
        let mut wanted: HashSet<TfDir> = HashSet::new();
        let terraform = self.config.contexts.terraform;
        for p in self.panes.values_mut() {
            if let Some(TfSeen::Dir { from, dir, data_dir }) = &p.seen.tf_workspace {
                if p.current_cwd().as_ref() == Some(from) {
                    wanted.insert((dir.clone(), data_dir.clone()));
                } else {
                    p.seen.tf_workspace = None;
                }
            }
            let shell_here = p.is_terminal() && p.ssh.is_none() && p.exit_code.is_none();
            if terraform && shell_here && !matches!(p.seen.tf_workspace, Some(TfSeen::Env(_))) {
                wanted.extend(p.current_cwd().map(|cwd| (cwd, None)));
            }
        }
        // What a directory was found to be is kept after the panes left it: coming back, a pane
        // finds it until the next read, instead of nothing (a production workspace would pass
        // unguarded until then). Only when many are kept do the unused ones that are not
        // Terraform directories go; a Terraform directory is never forgotten (there are few).
        forget_unused(&mut self.tf_dirs.read, &wanted);
        for key in wanted {
            if self.tf_dirs.reading.contains_key(&key) {
                continue;
            }
            self.tf_dirs.reads += 1;
            self.tf_dirs.reading.insert(key.clone(), (Instant::now(), self.tf_dirs.reads));
            let (read, done) = (key.clone(), key.clone());
            let started = self.background(
                move || context::terraform_dir_workspace(&read.0, read.1.as_deref()),
                move |server, ws| {
                    server.tf_dirs.reading.remove(&done);
                    if server.tf_dirs.read.get(&done) != Some(&ws) {
                        server.tf_dirs.read.insert(done, ws);
                        server.mark_dirty();
                    }
                },
            );
            if !started {
                self.tf_dirs.reading.remove(&key);
            }
        }
    }

    pub fn pane_terraform(&self, id: PaneId) -> Option<String> {
        let p = self.panes.get(&id)?;
        if !self.config.contexts.terraform || p.ssh.is_some() {
            return None;
        }
        let cwd = p.current_cwd();
        match &p.seen.tf_workspace {
            Some(TfSeen::Env(ws)) => Some(ws.clone()),
            // What a run worked on, while the pane is still where it ran from; not read yet, it
            // is unknown (see `pane_terraform_unknown`).
            Some(TfSeen::Dir { from, dir, data_dir }) if cwd.as_ref() == Some(from) => {
                match self.tf_dirs.read.get(&(dir.clone(), data_dir.clone()))? {
                    Some(ws) => Some(ws.clone()),
                    None => self.tf_dirs.read.get(&(cwd?, None))?.clone(),
                }
            }
            _ => self.tf_dirs.read.get(&(cwd?, None))?.clone(),
        }
    }

    /// The directory a Terraform run the pane made worked on (with its data directory), while
    /// the pane is still where it ran from.
    fn pane_tf_dir(&self, id: PaneId) -> Option<TfDir> {
        let p = self.panes.get(&id)?;
        if !self.config.contexts.terraform || p.ssh.is_some() {
            return None;
        }
        let Some(TfSeen::Dir { from, dir, data_dir }) = &p.seen.tf_workspace else { return None };
        (p.current_cwd().as_ref() == Some(from)).then(|| (dir.clone(), data_dir.clone()))
    }

    /// The directories whose workspace [`Server::pane_terraform`] may take: the one a run the
    /// pane made worked on, and the one it is in (unless `TF_WORKSPACE` says).
    fn pane_tf_keys(&self, id: PaneId) -> impl Iterator<Item = TfDir> {
        let here = self.panes.get(&id).filter(|p| {
            self.config.contexts.terraform && p.ssh.is_none() && !matches!(p.seen.tf_workspace, Some(TfSeen::Env(_)))
        });
        self.pane_tf_dir(id).into_iter().chain(here.and_then(|p| p.current_cwd()).map(|cwd| (cwd, None)))
    }

    /// Why the Terraform workspace of the pane is not known: a directory it comes from does not
    /// answer. It may well be production.
    fn pane_terraform_unknown(&self, id: PaneId) -> Option<Production> {
        self.pane_tf_keys(id).find_map(|key| {
            let (since, number) = self.tf_dirs.reading.get(&key)?;
            (since.elapsed() > CONTEXT_STALE).then(|| {
                Production::new(
                    format!("an unknown Terraform workspace ({} does not answer)", tf_place(&key)),
                    format!("stalled terraform {key:?} read #{number}"),
                )
            })
        })
    }

    /// What is not known yet about the pane's contexts because it was only just seen (a
    /// kubeconfig, a Terraform directory being read for the first time). Each is about what is
    /// read and the read: an approval given for one is not taken for another.
    fn pane_context_pending(&self, id: PaneId) -> Option<Production> {
        if let Some(why) = self.pane_kube_pending(id) {
            return Some(why);
        }
        let key = self.pane_tf_dir(id)?;
        if self.tf_dirs.read.contains_key(&key) {
            return None;
        }
        let number = self.tf_dirs.reading.get(&key).map_or(0, |(_, n)| *n);
        Some(Production::new(
            format!("a Terraform workspace not read yet ({})", tf_place(&key)),
            format!("pending terraform {key:?} read #{number}"),
        ))
    }

    pub fn pane_aws(&self, id: PaneId) -> Option<String> {
        let p = self.panes.get(&id)?;
        if !self.config.contexts.aws || p.ssh.is_some() {
            return None;
        }
        match &p.seen.aws_profile {
            Some(seen) => seen.clone(),
            None => spec_env(p, "AWS_PROFILE"),
        }
    }

    /// The server a pane is connected to: the connection as it is (host, user, port, jump host,
    /// key: what a tool must go through), with the groups, env and note of the known server it
    /// was named after. The known entry may reach that machine another way (another account,
    /// another jump host): its connection is not used.
    pub fn pane_server_entry(&self, id: PaneId) -> Option<crate::servers::Server> {
        let mut s = self.panes.get(&id)?.ssh.clone()?;
        if let Some(k) = self.known_servers.iter().find(|k| k.server.name == s.name) {
            s.groups = k.server.groups.clone();
            s.env = k.server.env.clone();
            s.note = k.server.note.clone();
        }
        Some(s)
    }

    /// Why a pane is production (`server web-prod-1`), if it may be: what decides whether to
    /// ask before acting there. A context just seen and not read yet counts as unknown, which
    /// may be production.
    pub fn pane_production(&self, id: PaneId) -> Option<Production> {
        if self.config.production.words.is_empty() {
            return None;
        }
        self.pane_production_known(id).or_else(|| self.pane_context_pending(id))
    }

    /// Why a pane is production, from what is known of it: what its frame shows and events
    /// say, which a context read a moment later must not make flicker.
    pub fn pane_production_shown(&self, id: PaneId) -> Option<String> {
        self.pane_production_known(id).map(|p| p.why)
    }

    fn pane_production_known(&self, id: PaneId) -> Option<Production> {
        let words = &self.config.production.words;
        if words.is_empty() {
            return None;
        }
        if let Some(s) = self.pane_server_entry(id) {
            return context::server_production(&s, words)
                .then(|| Production::new(format!("server {}", s.name), format!("server {s:?}")));
        }
        // A read that does not answer comes first: the context known before it may be gone.
        if let Some(stalled) = self.pane_kube_stalled(id) {
            return Some(stalled);
        }
        if let Some(k) = self.pane_kube(id) {
            let value = self.kubeconfig_of(id).unwrap_or_default();
            let what = self.kube_what(id, &value, self.kube_cache.known_since(&value).unwrap_or(0));
            if context::names_production(&k.context, words) {
                return Some(Production::new(format!("Kubernetes context {}", k.context), what));
            }
            if let Some(ns) = k.namespace.filter(|ns| context::names_production(ns, words)) {
                // The cluster too: the same namespace on two clusters is not one approval.
                return Some(Production::new(format!("Kubernetes namespace {ns} in {}", k.context), what));
            }
        }
        if let Some(why) = self.pane_kube_unknown(id) {
            return Some(why);
        }
        // As for Kubernetes: a read that does not answer (of the run's directory, or of the one
        // the pane is in) comes before the workspace known from the read before it.
        if let Some(why) = self.pane_terraform_unknown(id) {
            return Some(why);
        }
        if let Some(ws) = self.pane_terraform(id).filter(|ws| context::names_production(ws, words)) {
            let place = self
                .pane_tf_dir(id)
                .map(|k| format!("{k:?}"))
                .or_else(|| self.panes.get(&id).and_then(|p| p.current_cwd()).map(|d| format!("{d:?}")));
            return Some(Production::new(format!("Terraform workspace {ws}"), format!("terraform {place:?} {ws}")));
        }
        if let Some(p) = self.pane_aws(id).filter(|p| context::names_production(p, words)) {
            return Some(Production::new(format!("AWS profile {p}"), format!("aws {p}")));
        }
        None
    }

    /// Frame labels of a pane's contexts besides git (`tf:prod`, `⎈ prod-eks/payments`,
    /// `aws:ops`), each with whether it names production.
    pub fn context_labels(&self, id: PaneId) -> Vec<(String, bool)> {
        let ascii = crate::theme::current().ascii;
        let words = &self.config.production.words;
        let mut out = Vec::new();
        if let Some(ws) = self.pane_terraform(id) {
            out.push((format!("tf:{ws}"), context::names_production(&ws, words)));
        }
        if let Some(k) = self.pane_kube(id) {
            let prod = context::names_production(&k.context, words)
                || k.namespace.as_deref().is_some_and(|ns| context::names_production(ns, words));
            out.push((k.label(ascii), prod));
        } else if self.pane_kube_is_unknown(id) {
            out.push((if ascii { "k8s:?" } else { "⎈ ?" }.to_string(), true));
        }
        if let Some(p) = self.pane_aws(id) {
            out.push((format!("aws:{p}"), context::names_production(&p, words)));
        }
        out
    }

    /// Everything a tool started from this pane gets to know.
    pub fn tool_context(&self, id: PaneId) -> ToolContext {
        let Some(p) = self.panes.get(&id) else { return ToolContext::default() };
        let server = self.pane_server_entry(id);
        ToolContext {
            cwd: if server.is_none() { p.current_cwd() } else { None },
            remote_cwd: server.as_ref().and_then(|_| p.remote_cwd()),
            server,
            git: self.pane_git(id).cloned(),
            kube: self.pane_kube(id),
            kubeconfig: self.pane_kubeconfig(p),
            terraform: self.pane_terraform(id),
            aws_profile: self.pane_aws(id),
            aws_default: matches!(p.seen.aws_profile, Some(None)),
            selection: p.selection_text(),
            production: self.pane_production(id).map(|p| p.why),
            extra: Vec::new(),
            secrets: Vec::new(),
        }
    }

    /// Forget the server of panes where an interactive `ssh` ended.
    pub(super) fn forget_ended_ssh(&mut self, ids: Vec<PaneId>) {
        for id in ids {
            let Some(p) = self.panes.get(&id) else { continue };
            if self.ssh_target_of(&p.spec).is_none() {
                self.panes.get_mut(&id).unwrap().ssh = None;
                self.mark_dirty();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_terraform_directories_are_never_forgotten() {
        let dir = |n: usize| (PathBuf::from(format!("/d{n}")), None);
        let mut read: HashMap<TfDir, Option<String>> = (0..TF_DIRS_KEPT).map(|n| (dir(n), None)).collect();
        read.insert(dir(9999), Some("prod".into()));
        read.insert(dir(10000), None);
        let wanted: HashSet<TfDir> = [dir(10000)].into_iter().collect();
        forget_unused(&mut read, &wanted);
        assert_eq!(read.get(&dir(9999)), Some(&Some("prod".into())), "a Terraform directory stays");
        assert!(read.contains_key(&dir(10000)), "a wanted directory stays");
        assert_eq!(read.len(), 2, "the unused others go");
    }

    #[test]
    fn an_approval_holds_for_what_it_was_given_for() {
        let flags = |c: &str| context::KubeFlags { context: Some(c.into()), ..Default::default() };
        let stalled = |value: &str, f: &context::KubeFlags, read| {
            Production::new(
                "an unknown Kubernetes context (k does not answer)",
                format!("stalled {}", kube_what(value, f, read)),
            )
        };
        let asked = stalled("/k", &flags("prod-a"), 3);
        assert_eq!(asked, stalled("/k", &flags("prod-a"), 3), "the same stalled file asked about twice");
        assert_ne!(asked, stalled("/k", &flags("prod-b"), 3), "the same words, other flags");
        assert_ne!(asked, stalled("/k", &flags("prod-a"), 4), "a newer read");
        assert_ne!(asked, stalled("/other", &flags("prod-a"), 3), "another file");
        // Two kubeconfigs may name a context alike: they are not one approval.
        let a = Production::new("Kubernetes context prod", kube_what("/a", &Default::default(), 1));
        let b = Production::new("Kubernetes context prod", kube_what("/b", &Default::default(), 1));
        assert_ne!(a, b);
        assert_eq!(a.to_string(), "Kubernetes context prod");
        assert_eq!(a.clone().and(a.clone()), a);
        assert_eq!(a.and(b).to_string(), "Kubernetes context prod; Kubernetes context prod");
    }
}
