//! User configuration (`~/.config/kraal/config.toml`).
//!
//! [`DEFAULT_CONFIG`] is the single source of truth for defaults: the user's file is merged over
//! it key by key, so a config only needs the settings it changes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths;

pub const DEFAULT_CONFIG: &str = r##"# kraal configuration. Every setting is optional; this file shows the defaults.
# Print it again with `kraal config default`.

# Shell for new panes (defaults to $SHELL).
# shell = "/bin/zsh"

# Lines of scrollback kept per pane.
scrollback = 10000

# Draw a frame with a title around every pane.
pane_frames = true

# Capture the mouse: click to focus, drag borders, wheel to scroll, drag to select and copy.
# Hold Shift to use your terminal's own selection instead.
mouse = true

# Workspace definitions (*.kdl, Zellij layout compatible) are read from
# ~/.config/kraal/workspaces, plus these directories.
workspace_dirs = []

# Offer the hosts of ~/.ssh/config in the SSH picker.
ssh_config_hosts = true

[servers]
# Remember servers you ssh to that kraal does not know yet (saved in ~/.config/kraal/servers.toml,
# group "discovered").
auto_add = true
# Share one SSH connection per server between panes, file transfers and file panes, so a
# second pane or a copy to the same server starts instantly without asking for a password.
multiplex = true
# Opening a whole group puts at most this many servers per tab.
max_panes_per_tab = 9
# Where downloads from a server go by default.
download_dir = "~/Downloads"

[theme]
# "default", "high-contrast" or "ascii" (no box-drawing characters or symbols).
preset = "default"
# Override colours with a name (green, lightblue…), "#rrggbb" or a palette index (0-255):
# accent = "green"
# dim = "darkgray"
# sync = "magenta"
# production = "red"
# Pane frames: "rounded", "plain", "thick", "double" or "ascii".
# frames = "rounded"

[sidebar]
# Shown when it has something to show (other workspaces, agents, ports, tunnels); false hides it.
visible = true
width = 28

[notifications]
# "terminal" (OSC 777 / OSC 9, shown by Ghostty, iTerm2, kitty…), "system" (notify-send),
# "both" or "off".
mode = "terminal"
on_blocked = true
on_done = true

[contexts]
# Show in pane frames what a pane works on besides git:
# the Kubernetes context of panes that ran kubectl, k9s, helm…,
kubernetes = true
# the Terraform workspace of panes in a directory where `terraform init` ran,
terraform = true
# and the AWS profile of panes that ran aws, terraform, cdk… with AWS_PROFILE set.
aws = true

[production]
# A pane is production when its server, Kubernetes context or namespace, Terraform workspace or
# AWS profile has one of these words in its name ("web-prod-1", "prod-eks", "db.prod.lan"), or
# when its server has env = "prod" in servers.toml (a server with another env never is).
# Production panes get a red frame and a PROD badge. [] turns all of this off.
words = ["prod", "production", "prd"]
# Ask before keys typed in sync reach production panes (once per pane, until sync stops).
confirm_sync = true
# Ask before pasting several lines into a production pane that would run them at once.
confirm_paste = true

# Tools: programs one key away, started where the focused pane is (ctrl+b T lists them).
#   command  a shell command line (run by sh -c). The pane's context is in $KRAAL_CWD,
#            $KRAAL_SERVER, $KRAAL_HOST, $KRAAL_REPO, $KRAAL_BRANCH, $KRAAL_KUBE_CONTEXT,
#            $KRAAL_KUBE_NAMESPACE, $KRAAL_TF_WORKSPACE, $KRAAL_AWS_PROFILE, $KRAAL_SELECTION
#            and $KRAAL_PRODUCTION ("1" or empty).
#   on       "pane": on the pane's server over SSH, or here in its directory (default);
#            "local": always on this machine, with the pane's KUBECONFIG and AWS_PROFILE.
#   open     "floating" (default), "right", "down" or "tab".
#   hold     keep the pane open after the command ends, to read its output (a failing
#            command always stays open).
#   when     what the pane needs for the tool to be offered: "git", "server", "local",
#            "kubernetes", "terraform", "aws", "selection".
#   key      a key after the prefix that starts the tool directly, e.g. "i".
#   read_only  true when the tool only looks: it starts on a production pane without asking.
#   enabled  false hides a tool.
# Tools that run here are hidden when their program is not installed. One of your own:
#   [tools.pods]
#   description = "pods of the pane's cluster and namespace"
#   command = "kubectl get pods -o wide"
#   on = "local"
#   when = ["kubernetes"]
#   hold = true

[tools.lazygit]
description = "git: stage, commit, branches, history"
command = "lazygit"
on = "local"
when = ["git"]

[tools.k9s]
description = "Kubernetes: the pane's cluster"
command = 'k9s ${KRAAL_KUBE_CONTEXT:+--context "$KRAAL_KUBE_CONTEXT"} ${KRAAL_KUBE_NAMESPACE:+-n "$KRAAL_KUBE_NAMESPACE"}'
on = "local"

[tools.htop]
description = "processes of the pane's machine"
command = "htop"
read_only = true

[tools.logs]
description = "system logs of the pane's machine"
command = "journalctl -f -n 200"
read_only = true

[tools.docker]
description = "containers of the pane's machine"
command = "docker ps -a"
hold = true
read_only = true

[tools.disk]
description = "disk space of the pane's machine"
command = "df -h"
hold = true
read_only = true

[hints]
# ctrl+b space labels what can be acted on in the focused pane: links, paths (with :line),
# commits, addresses, IPs and known servers, Kubernetes pods, UUIDs. Type a label to copy it;
# type it with Shift to act where the pane is: open a link (a port on the pane's server gets a
# tunnel), edit a file at its line, `git show` a commit (or follow a container's logs), ssh to a
# server, follow a pod's logs, type a UUID.
# Letters labels are made of, easiest first.
alphabet = "asdfqwerzxcvjklmiuopghtybn"
# Patterns of your own; Shift+label runs `tool` with the match in $KRAAL_SELECTION:
#   [hints.patterns.ticket]
#   pattern = 'OPS-\d+'
#   tool = "jira"

[hooks]
# Run a shell command when something happens. Keys are event types (`kraal events --list`), or a
# prefix with ".*" ("agent.*"). The command gets $KRAAL_EVENT, $KRAAL_EVENT_JSON and the event's
# fields as $KRAAL_<FIELD> ($KRAAL_PANE, $KRAAL_NAME, $KRAAL_SERVER, $KRAAL_CODE…), and can call
# kraal (`kraal read $KRAAL_PANE`). Its output goes to ~/.local/state/kraal/hooks.log.
#   "agent.blocked" = 'notify-send "$KRAAL_AGENT needs you" "$KRAAL_NAME"'
#   "pane.failed" = 'curl -s -d "$KRAAL_NAME failed ($KRAAL_CODE)" ntfy.sh/my-topic'
#   "ssh.connected" = 'logger -t kraal "ssh $KRAAL_HOST production=$KRAAL_PRODUCTION"'

[keys]
# "kraal": the prefix opens a menu of commands grouped by what they act on: p panes, t tabs,
# w workspaces, a agents, s servers, f files, r run, v view, b broadcast. Common ones need one
# key (n new pane, x close, z zoom, arrows focus, 1-9 tabs, space search commands, h help).
# "classic": tmux's single keys after the prefix (% " x z c n p o s…).
preset = "kraal"
# The prefix (leader) key(s). Pressing it twice sends it to the program.
prefix = ["ctrl+b"]

# Change a command's keys after the prefix: action = ["key", "key key"] (a space separates the
# keys of a sequence: "p right" is p, then →); [] unbinds it. Names: `kraal config keys`.
[keys.leader]
# split_right = ["%", "p right"]

# Keys that act without the prefix (in the "kraal" preset: alt+arrows focus, alt+n new pane,
# alt+a next agent needing you).
[keys.direct]
# toggle_floating = ["alt+f"]
"##;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub shell: Option<String>,
    pub scrollback: usize,
    pub pane_frames: bool,
    pub mouse: bool,
    pub workspace_dirs: Vec<String>,
    pub ssh_config_hosts: bool,
    pub servers: ServersConfig,
    #[serde(default)]
    pub theme: crate::theme::ThemeConfig,
    pub sidebar: SidebarConfig,
    pub notifications: NotificationConfig,
    pub contexts: ContextsConfig,
    pub production: ProductionConfig,
    pub hints: crate::hints::HintsConfig,
    #[serde(default)]
    pub hooks: toml::Table,
    #[serde(default)]
    pub tools: BTreeMap<String, crate::tools::ToolDef>,
    pub keys: crate::keymap::KeysConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextsConfig {
    pub kubernetes: bool,
    pub terraform: bool,
    pub aws: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProductionConfig {
    pub words: Vec<String>,
    pub confirm_sync: bool,
    pub confirm_paste: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServersConfig {
    pub auto_add: bool,
    pub multiplex: bool,
    pub max_panes_per_tab: usize,
    pub download_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SidebarConfig {
    pub visible: bool,
    pub width: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyMode {
    Terminal,
    System,
    Both,
    Off,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotificationConfig {
    pub mode: NotifyMode,
    pub on_blocked: bool,
    pub on_done: bool,
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str(DEFAULT_CONFIG).expect("default config is valid")
    }
}

impl Config {
    /// Load the user's config merged over the defaults. Missing file = defaults.
    pub fn load() -> Result<Config> {
        let path = paths::config_path();
        let user = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::from_user_toml(&user).with_context(|| format!("in {}", path.display()))
    }

    pub fn from_user_toml(user: &str) -> Result<Config> {
        let mut base: toml::Value = toml::from_str(DEFAULT_CONFIG)?;
        let user: toml::Value = toml::from_str(user)?;
        merge(&mut base, user);
        Ok(base.try_into()?)
    }

    pub fn shell(&self) -> String {
        self.shell.clone().or_else(|| std::env::var("SHELL").ok()).unwrap_or_else(|| "/bin/sh".into())
    }

    pub fn workspace_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![paths::workspaces_dir()];
        for d in &self.workspace_dirs {
            let d = paths::expand_tilde(d);
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        dirs
    }
}

fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) if slot.is_table() && v.is_table() => merge(slot, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let cfg = Config::default();
        assert_eq!(cfg.scrollback, 10000);
        assert_eq!(cfg.keys.preset, "kraal");
        assert!(cfg.keys.leader.is_empty() && cfg.keys.direct.is_empty(), "the preset has the bindings");
    }

    #[test]
    fn user_config_merges_over_defaults() {
        let cfg = Config::from_user_toml(
            "pane_frames = false\n[keys]\nprefix = [\"ctrl+a\"]\n[keys.direct]\nnew_pane = []\n",
        )
        .unwrap();
        assert!(!cfg.pane_frames);
        assert_eq!(cfg.keys.prefix, vec!["ctrl+a".to_string()]);
        assert!(cfg.keys.direct["new_pane"].is_empty());
        // untouched defaults survive
        assert_eq!(cfg.keys.preset, "kraal");
        assert!(cfg.sidebar.visible);
    }

    #[test]
    fn tools_parse() {
        let cfg = Config::from_user_toml(
            "[tools.lazygit]\nkey = \"i\"\n[tools.k9s]\nenabled = false\n[tools.mine]\ncommand = \"tig\"\nkey = \"x\"\n",
        )
        .unwrap();
        assert_eq!(cfg.tools["lazygit"].command, "lazygit", "user keys merge into a default tool");
        assert!(!cfg.tools["k9s"].enabled);
        assert!(crate::tools::check(&cfg.tools).is_empty());
        assert!(Config::from_user_toml("[tools.bad]\ncommand = \"x\"\nwhen = [\"git\", \"nope\"]\n").is_err());
        assert_eq!(cfg.production.words, vec!["prod", "production", "prd"]);
    }
}
