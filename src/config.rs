//! User configuration (`~/.config/kraal/config.toml`).
//!
//! [`DEFAULT_CONFIG`] is the single source of truth for defaults: the user's file is merged over
//! it key by key, so a config only needs the settings it changes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::input::{Key, KeySpec};
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
# Pane frames: "rounded", "plain", "thick", "double" or "ascii".
# frames = "rounded"

[sidebar]
visible = true
width = 28

[notifications]
# "terminal" (OSC 777 / OSC 9, shown by Ghostty, iTerm2, kitty…), "system" (notify-send),
# "both" or "off".
mode = "terminal"
on_blocked = true
on_done = true

[keys]
# Prefix key(s), tmux style. Press the prefix, then one of the [keys.prefixed] keys.
prefix = ["ctrl+b"]

# Keys that act without the prefix. Map an action to a list of keys; [] disables it.
[keys.direct]
focus_or_tab_left = ["alt+h", "alt+left"]
focus_or_tab_right = ["alt+l", "alt+right"]
focus_up = ["alt+k", "alt+up"]
focus_down = ["alt+j", "alt+down"]
new_pane = ["alt+n"]
toggle_floating = ["alt+f"]
next_attention = ["alt+a"]
workspace_picker = ["alt+o"]
agent_picker = ["alt+g"]

# Keys that act after the prefix.
[keys.prefixed]
split_right = ["%", "|"]
split_down = ['"', "-"]
close_pane = ["x"]
toggle_zoom = ["z"]
toggle_floating = ["t"]
new_floating = ["F"]
files = ["f"]
focus_left = ["h", "left"]
focus_right = ["l", "right"]
focus_up = ["k", "up"]
focus_down = ["j", "down"]
resize_left = ["H", "ctrl+left"]
resize_right = ["L", "ctrl+right"]
resize_up = ["K", "ctrl+up"]
resize_down = ["J", "ctrl+down"]
new_tab = ["c"]
next_tab = ["n", "tab"]
prev_tab = ["p", "shift+tab"]
close_tab = ["&"]
rename_tab = [","]
rename_pane = ["."]
rename_workspace = ["$"]
tab_1 = ["1"]
tab_2 = ["2"]
tab_3 = ["3"]
tab_4 = ["4"]
tab_5 = ["5"]
tab_6 = ["6"]
tab_7 = ["7"]
tab_8 = ["8"]
tab_9 = ["9"]
next_workspace = [")"]
prev_workspace = ["("]
workspace_picker = ["o", "w"]
ssh_picker = ["s"]
upload = ["U"]
download = ["D"]
pane_picker = ["g"]
agent_picker = ["G"]
switch_account = ["A"]
git_actions = ["R"]
next_attention = ["a"]
toggle_sidebar = ["b"]
toggle_sync = ["y"]
mark_pane = ["m"]
scroll_mode = ["[", "PageUp", "v"]
search = ["/"]
open_preview = ["e"]
save_workspace = ["S"]
detach = ["d", "q"]
help = ["?"]
send_prefix = ["ctrl+b"]
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
    pub keys: KeysConfig,
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

#[derive(Debug, Clone, Deserialize)]
pub struct KeysConfig {
    pub prefix: Vec<String>,
    pub direct: BTreeMap<String, Vec<String>>,
    pub prefixed: BTreeMap<String, Vec<String>>,
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

/// Everything a key can trigger. Declaration order is the order of the help screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    NewPane,
    SplitRight,
    SplitDown,
    ClosePane,
    ToggleZoom,
    ToggleFloating,
    NewFloating,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    FocusOrTabLeft,
    FocusOrTabRight,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    NewTab,
    NextTab,
    PrevTab,
    CloseTab,
    RenameTab,
    RenamePane,
    RenameWorkspace,
    Tab(u8),
    NextWorkspace,
    PrevWorkspace,
    WorkspacePicker,
    SshPicker,
    Upload,
    Download,
    Files,
    PanePicker,
    AgentPicker,
    SwitchAccount,
    GitActions,
    NextAttention,
    ToggleSidebar,
    ToggleSync,
    MarkPane,
    ScrollMode,
    Search,
    OpenPreview,
    SaveWorkspace,
    Detach,
    Help,
    SendPrefix,
}

impl Action {
    pub fn parse(name: &str) -> Option<Action> {
        use Action::*;
        Some(match name {
            "new_pane" => NewPane,
            "split_right" => SplitRight,
            "split_down" => SplitDown,
            "close_pane" => ClosePane,
            "toggle_zoom" => ToggleZoom,
            "toggle_floating" => ToggleFloating,
            "new_floating" => NewFloating,
            "focus_left" => FocusLeft,
            "focus_right" => FocusRight,
            "focus_up" => FocusUp,
            "focus_down" => FocusDown,
            "focus_or_tab_left" => FocusOrTabLeft,
            "focus_or_tab_right" => FocusOrTabRight,
            "resize_left" => ResizeLeft,
            "resize_right" => ResizeRight,
            "resize_up" => ResizeUp,
            "resize_down" => ResizeDown,
            "new_tab" => NewTab,
            "next_tab" => NextTab,
            "prev_tab" => PrevTab,
            "close_tab" => CloseTab,
            "rename_tab" => RenameTab,
            "rename_pane" => RenamePane,
            "rename_workspace" => RenameWorkspace,
            "next_workspace" => NextWorkspace,
            "prev_workspace" => PrevWorkspace,
            "workspace_picker" => WorkspacePicker,
            "ssh_picker" => SshPicker,
            "upload" => Upload,
            "download" => Download,
            "files" => Files,
            "pane_picker" => PanePicker,
            "agent_picker" => AgentPicker,
            "switch_account" => SwitchAccount,
            "git_actions" => GitActions,
            "next_attention" => NextAttention,
            "toggle_sidebar" => ToggleSidebar,
            "toggle_sync" => ToggleSync,
            "mark_pane" => MarkPane,
            "scroll_mode" => ScrollMode,
            "search" => Search,
            "open_preview" => OpenPreview,
            "save_workspace" => SaveWorkspace,
            "detach" => Detach,
            "help" => Help,
            "send_prefix" => SendPrefix,
            other => {
                let n: u8 = other.strip_prefix("tab_")?.parse().ok()?;
                if (1..=9).contains(&n) {
                    Tab(n)
                } else {
                    return None;
                }
            }
        })
    }

    /// Short description for the help screen.
    pub fn describe(self) -> String {
        use Action::*;
        match self {
            NewPane => "new pane".into(),
            SplitRight => "split right".into(),
            SplitDown => "split down".into(),
            ClosePane => "close pane".into(),
            ToggleZoom => "zoom pane (fullscreen)".into(),
            ToggleFloating => "show/hide floating panes".into(),
            NewFloating => "new floating pane".into(),
            FocusLeft => "focus left".into(),
            FocusRight => "focus right".into(),
            FocusUp => "focus up".into(),
            FocusDown => "focus down".into(),
            FocusOrTabLeft => "focus left / previous tab".into(),
            FocusOrTabRight => "focus right / next tab".into(),
            ResizeLeft => "resize left".into(),
            ResizeRight => "resize right".into(),
            ResizeUp => "resize up".into(),
            ResizeDown => "resize down".into(),
            NewTab => "new tab".into(),
            NextTab => "next tab".into(),
            PrevTab => "previous tab".into(),
            CloseTab => "close tab".into(),
            RenameTab => "rename tab".into(),
            RenamePane => "rename pane".into(),
            RenameWorkspace => "rename workspace".into(),
            Tab(n) => format!("go to tab {n}"),
            NextWorkspace => "next workspace".into(),
            PrevWorkspace => "previous workspace".into(),
            WorkspacePicker => "workspaces".into(),
            SshPicker => "servers: connect, open a group".into(),
            Upload => "upload files to this pane's server".into(),
            Download => "download from this pane's server".into(),
            Files => "file manager (local ⇄ servers)".into(),
            PanePicker => "find a pane".into(),
            AgentPicker => "agents: start one on an account".into(),
            SwitchAccount => "continue this agent on another account".into(),
            GitActions => "worktree: review, commit, pull request, merge, remove".into(),
            NextAttention => "next agent needing you".into(),
            ToggleSidebar => "sidebar".into(),
            ToggleSync => "sync typing: tab → workspace → off".into(),
            MarkPane => "mark this pane for synced typing".into(),
            ScrollMode => "scroll / copy mode".into(),
            Search => "search the scrollback".into(),
            OpenPreview => "preview a file".into(),
            SaveWorkspace => "save workspace layout".into(),
            Detach => "detach".into(),
            Help => "help".into(),
            SendPrefix => "send the prefix key".into(),
        }
    }
}

/// Resolved key bindings.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    pub prefix: Vec<KeySpec>,
    pub direct: Vec<(KeySpec, Action)>,
    pub prefixed: Vec<(KeySpec, Action)>,
}

impl Keymap {
    /// Build from config; returns the keymap and warnings about invalid entries.
    pub fn build(cfg: &KeysConfig) -> (Keymap, Vec<String>) {
        fn parse_list(list: &[String], ctx: &str, warnings: &mut Vec<String>) -> Vec<KeySpec> {
            list.iter()
                .filter_map(|s| match KeySpec::parse(s) {
                    Ok(k) => Some(k),
                    Err(e) => {
                        warnings.push(format!("{ctx}: invalid key {s:?}: {e}"));
                        None
                    }
                })
                .collect()
        }
        fn table(map: &BTreeMap<String, Vec<String>>, ctx: &str, warnings: &mut Vec<String>) -> Vec<(KeySpec, Action)> {
            let mut out = Vec::new();
            for (name, keys) in map {
                let Some(action) = Action::parse(name) else {
                    warnings.push(format!("{ctx}: unknown action {name:?}"));
                    continue;
                };
                for k in parse_list(keys, ctx, warnings) {
                    out.push((k, action));
                }
            }
            out
        }
        let mut warnings = Vec::new();
        let prefix = parse_list(&cfg.prefix, "keys.prefix", &mut warnings);
        let direct = table(&cfg.direct, "keys.direct", &mut warnings);
        let prefixed = table(&cfg.prefixed, "keys.prefixed", &mut warnings);
        (Keymap { prefix, direct, prefixed }, warnings)
    }

    pub fn is_prefix(&self, key: &Key) -> bool {
        self.prefix.iter().any(|p| p.matches(key))
    }

    pub fn direct(&self, key: &Key) -> Option<Action> {
        self.direct.iter().find(|(k, _)| k.matches(key)).map(|(_, a)| *a)
    }

    pub fn prefixed(&self, key: &Key) -> Option<Action> {
        self.prefixed.iter().find(|(k, _)| k.matches(key)).map(|(_, a)| *a)
    }

    /// First key bound to an action, for hints (`"ctrl+b %"` style when prefixed).
    pub fn hint(&self, action: Action) -> Option<String> {
        if let Some((k, _)) = self.direct.iter().find(|(_, a)| *a == action) {
            return Some(k.to_string());
        }
        let (k, _) = self.prefixed.iter().find(|(_, a)| *a == action)?;
        Some(k.to_string())
    }

    pub fn prefix_label(&self) -> String {
        self.prefix.first().map(|k| k.to_string()).unwrap_or_else(|| "prefix".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_bind() {
        let cfg = Config::default();
        assert_eq!(cfg.scrollback, 10000);
        let (km, warnings) = Keymap::build(&cfg.keys);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(km.prefixed.iter().any(|(_, a)| *a == Action::Tab(9)));
        assert!(km.direct.iter().any(|(_, a)| *a == Action::NextAttention));
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
        assert_eq!(cfg.keys.direct["focus_up"], vec!["alt+k".to_string(), "alt+up".to_string()]);
        assert!(cfg.sidebar.visible);
    }

    #[test]
    fn every_default_action_name_is_known() {
        let cfg = Config::default();
        for name in cfg.keys.direct.keys().chain(cfg.keys.prefixed.keys()) {
            assert!(Action::parse(name).is_some(), "{name}");
        }
    }
}
