//! What keys do: every command kraal has, the presets that bind them, and the lookups the
//! server, its menu, its help and its command search share.
//!
//! Commands are reached with a leader key (`ctrl+b` by default) followed by a short sequence. The
//! "kraal" preset groups them by what they act on (`p` panes, `t` tabs, `w` workspaces, `a`
//! agents, `s` servers, `f` files, `r` run, `v` view, `b` broadcast) and shows a menu of what can
//! follow; the "classic" preset keeps tmux's single keys. A few frequent commands also have
//! direct keys that need no leader.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::input::{Key, KeyCode, KeySpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    ResizeMode,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    PanePicker,
    RenamePane,
    NewTab,
    NextTab,
    PrevTab,
    CloseTab,
    RenameTab,
    Tab(u8),
    WorkspacePicker,
    NewWorkspace,
    NextWorkspace,
    PrevWorkspace,
    RenameWorkspace,
    CloseWorkspace,
    SaveWorkspace,
    AgentPicker,
    NextAttention,
    SwitchAccount,
    SshPicker,
    EditServers,
    Upload,
    Download,
    Files,
    OpenPreview,
    ToolPicker,
    Runbooks,
    GitActions,
    ScrollMode,
    Search,
    Hints,
    ToggleSidebar,
    Messages,
    SelectForBroadcast,
    BroadcastSelected,
    BroadcastTab,
    BroadcastWorkspace,
    BroadcastOff,
    BroadcastToggle,
    CommandPalette,
    Help,
    Detach,
    SendPrefix,
}

use Action::*;

/// Every action (but `Tab(n)`): its name in the configuration, and what it does.
const ACTIONS: &[(Action, &str, &str)] = &[
    (NewPane, "new_pane", "New pane"),
    (SplitRight, "split_right", "Split right"),
    (SplitDown, "split_down", "Split down"),
    (ClosePane, "close_pane", "Close pane"),
    (ToggleZoom, "toggle_zoom", "Zoom pane"),
    (ToggleFloating, "toggle_floating", "Show or hide floating panes"),
    (NewFloating, "new_floating", "New floating pane"),
    (FocusLeft, "focus_left", "Focus left"),
    (FocusRight, "focus_right", "Focus right"),
    (FocusUp, "focus_up", "Focus up"),
    (FocusDown, "focus_down", "Focus down"),
    (FocusOrTabLeft, "focus_or_tab_left", "Focus left (previous tab at the edge)"),
    (FocusOrTabRight, "focus_or_tab_right", "Focus right (next tab at the edge)"),
    (ResizeMode, "resize_mode", "Resize panes"),
    (ResizeLeft, "resize_left", "Resize left"),
    (ResizeRight, "resize_right", "Resize right"),
    (ResizeUp, "resize_up", "Resize up"),
    (ResizeDown, "resize_down", "Resize down"),
    (PanePicker, "pane_picker", "Find a pane"),
    (RenamePane, "rename_pane", "Rename pane"),
    (NewTab, "new_tab", "New tab"),
    (NextTab, "next_tab", "Next tab"),
    (PrevTab, "prev_tab", "Previous tab"),
    (CloseTab, "close_tab", "Close tab"),
    (RenameTab, "rename_tab", "Rename tab"),
    (WorkspacePicker, "workspace_picker", "Choose a workspace"),
    (NewWorkspace, "new_workspace", "New workspace"),
    (NextWorkspace, "next_workspace", "Next workspace"),
    (PrevWorkspace, "prev_workspace", "Previous workspace"),
    (RenameWorkspace, "rename_workspace", "Rename workspace"),
    (CloseWorkspace, "close_workspace", "Close workspace"),
    (SaveWorkspace, "save_workspace", "Save workspace layout"),
    (AgentPicker, "agent_picker", "Start an agent"),
    (NextAttention, "next_attention", "Next agent needing you"),
    (SwitchAccount, "switch_account", "Change the agent's account"),
    (SshPicker, "ssh_picker", "Connect to a server"),
    (EditServers, "edit_servers", "Edit saved servers"),
    (Upload, "upload", "Upload to this pane's server"),
    (Download, "download", "Download from this pane's server"),
    (Files, "files", "File manager"),
    (OpenPreview, "open_preview", "Preview a file"),
    (ToolPicker, "tools", "Tools where this pane is"),
    (Runbooks, "runbooks", "Runbooks"),
    (GitActions, "git_actions", "Git and worktrees"),
    (ScrollMode, "scroll_mode", "Copy mode: scroll and select"),
    (Search, "search", "Search the output"),
    (Hints, "hints", "Hints: links, paths, commits on screen"),
    (ToggleSidebar, "toggle_sidebar", "Sidebar"),
    (Messages, "messages", "Messages"),
    (SelectForBroadcast, "select_for_broadcast", "Select this pane for broadcast"),
    (BroadcastSelected, "broadcast_selected", "Broadcast to the selected panes"),
    (BroadcastTab, "broadcast_tab", "Broadcast to this tab"),
    (BroadcastWorkspace, "broadcast_workspace", "Broadcast to this workspace"),
    (BroadcastOff, "broadcast_off", "Stop broadcasting"),
    (BroadcastToggle, "broadcast_toggle", "Broadcast to this tab, or stop"),
    (CommandPalette, "command_palette", "Search commands"),
    (Help, "help", "Help"),
    (Detach, "detach", "Detach (programs keep running)"),
    (SendPrefix, "send_prefix", "Send the prefix key"),
];

impl Action {
    pub fn parse(name: &str) -> Option<Action> {
        if let Some(&(a, _, _)) = ACTIONS.iter().find(|(_, n, _)| *n == name) {
            return Some(a);
        }
        // Names of earlier versions.
        match name {
            "toggle_sync" => return Some(BroadcastToggle),
            "mark_pane" => return Some(SelectForBroadcast),
            _ => {}
        }
        let n: u8 = name.strip_prefix("tab_")?.parse().ok()?;
        (1..=9).contains(&n).then_some(Tab(n))
    }

    pub fn name(self) -> String {
        match self {
            Tab(n) => format!("tab_{n}"),
            a => ACTIONS.iter().find(|(x, _, _)| *x == a).map(|(_, n, _)| n.to_string()).unwrap_or_default(),
        }
    }

    pub fn describe(self) -> String {
        match self {
            Tab(n) => format!("Go to tab {n}"),
            a => ACTIONS.iter().find(|(x, _, _)| *x == a).map(|(_, _, d)| d.to_string()).unwrap_or_default(),
        }
    }

    /// The description without its explanation (after a colon, or in parentheses): what the
    /// menu shows.
    pub fn label(self) -> String {
        let d = self.describe();
        let short = d.split(" (").next().unwrap_or(&d);
        short.split(':').next().unwrap_or(short).trim().to_string()
    }

    /// Every action, in the order of the reference (tabs 1-9 as one).
    pub fn all() -> impl Iterator<Item = Action> {
        ACTIONS.iter().map(|(a, _, _)| *a).chain(std::iter::once(Tab(1)))
    }
}

/// What a key sequence runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Action(Action),
    /// A tool of `[tools]`, by name.
    Tool(String),
}

type Table = &'static [(&'static str, &'static [&'static str])];

/// A set of default bindings.
pub struct Preset {
    pub name: &'static str,
    /// Sequences after the leader (keys separated by spaces), in menu order.
    leader: Table,
    direct: Table,
    /// Titles of the menus a first key opens.
    groups: &'static [(&'static str, &'static str)],
    /// What a tool's `key` follows.
    tool_menu: &'static str,
}

pub const KRAAL: Preset = Preset {
    name: "kraal",
    leader: &[
        ("command_palette", &["space"]),
        ("new_pane", &["n", "p n"]),
        ("focus_left", &["left"]),
        ("focus_down", &["down"]),
        ("focus_up", &["up"]),
        ("focus_right", &["right"]),
        ("close_pane", &["x", "p x"]),
        ("toggle_zoom", &["z", "p z"]),
        ("tab_1", &["1"]),
        ("tab_2", &["2"]),
        ("tab_3", &["3"]),
        ("tab_4", &["4"]),
        ("tab_5", &["5"]),
        ("tab_6", &["6"]),
        ("tab_7", &["7"]),
        ("tab_8", &["8"]),
        ("tab_9", &["9"]),
        ("help", &["h", "?"]),
        ("detach", &["d"]),
        ("send_prefix", &["ctrl+b"]),
        ("pane_picker", &["p p", "p enter"]),
        ("split_right", &["p right"]),
        ("split_down", &["p down"]),
        ("rename_pane", &["p e"]),
        ("resize_mode", &["p r"]),
        ("new_floating", &["p f"]),
        ("new_tab", &["t n"]),
        ("next_tab", &["t right"]),
        ("prev_tab", &["t left"]),
        ("rename_tab", &["t e"]),
        ("close_tab", &["t x"]),
        ("workspace_picker", &["w w", "w enter"]),
        ("new_workspace", &["w n"]),
        ("next_workspace", &["w right"]),
        ("prev_workspace", &["w left"]),
        ("rename_workspace", &["w e"]),
        ("save_workspace", &["w s"]),
        ("close_workspace", &["w x"]),
        ("agent_picker", &["a a", "a enter"]),
        ("next_attention", &["a n"]),
        ("switch_account", &["a c"]),
        ("ssh_picker", &["s s", "s enter"]),
        ("upload", &["s u", "f u"]),
        ("download", &["s d", "f d"]),
        ("edit_servers", &["s e"]),
        ("files", &["f f", "f enter"]),
        ("open_preview", &["f p"]),
        ("tools", &["r r", "r enter"]),
        ("runbooks", &["r b"]),
        ("git_actions", &["r g"]),
        ("scroll_mode", &["v c"]),
        ("search", &["v s"]),
        ("hints", &["v h"]),
        ("toggle_floating", &["v f"]),
        ("toggle_sidebar", &["v b"]),
        ("messages", &["v m"]),
        ("select_for_broadcast", &["b m"]),
        ("broadcast_selected", &["b s"]),
        ("broadcast_tab", &["b t"]),
        ("broadcast_workspace", &["b w"]),
        ("broadcast_off", &["b o"]),
    ],
    direct: &[
        ("focus_or_tab_left", &["alt+left"]),
        ("focus_or_tab_right", &["alt+right"]),
        ("focus_up", &["alt+up"]),
        ("focus_down", &["alt+down"]),
        ("new_pane", &["alt+n"]),
        ("next_attention", &["alt+a"]),
    ],
    groups: &[
        ("p", "Panes"),
        ("t", "Tabs"),
        ("w", "Workspaces"),
        ("a", "Agents"),
        ("s", "Servers"),
        ("f", "Files"),
        ("r", "Run"),
        ("v", "View"),
        ("b", "Broadcast"),
    ],
    tool_menu: "r",
};

/// tmux's keys, as kraal had them first.
pub const CLASSIC: Preset = Preset {
    name: "classic",
    leader: &[
        ("split_right", &["%", "|"]),
        ("split_down", &["\"", "-"]),
        ("close_pane", &["x"]),
        ("toggle_zoom", &["z"]),
        ("toggle_floating", &["t"]),
        ("new_floating", &["F"]),
        ("files", &["f"]),
        ("focus_left", &["h", "left"]),
        ("focus_right", &["l", "right"]),
        ("focus_up", &["k", "up"]),
        ("focus_down", &["j", "down"]),
        ("resize_left", &["H", "ctrl+left"]),
        ("resize_right", &["L", "ctrl+right"]),
        ("resize_up", &["K", "ctrl+up"]),
        ("resize_down", &["J", "ctrl+down"]),
        ("new_tab", &["c"]),
        ("next_tab", &["n", "tab"]),
        ("prev_tab", &["p", "shift+tab"]),
        ("close_tab", &["&"]),
        ("rename_tab", &[","]),
        ("rename_pane", &["."]),
        ("rename_workspace", &["$"]),
        ("tab_1", &["1"]),
        ("tab_2", &["2"]),
        ("tab_3", &["3"]),
        ("tab_4", &["4"]),
        ("tab_5", &["5"]),
        ("tab_6", &["6"]),
        ("tab_7", &["7"]),
        ("tab_8", &["8"]),
        ("tab_9", &["9"]),
        ("next_workspace", &[")"]),
        ("prev_workspace", &["("]),
        ("workspace_picker", &["o", "w"]),
        ("ssh_picker", &["s"]),
        ("upload", &["U"]),
        ("download", &["D"]),
        ("pane_picker", &["g"]),
        ("agent_picker", &["G"]),
        ("switch_account", &["A"]),
        ("git_actions", &["R"]),
        ("tools", &["T"]),
        ("hints", &["space"]),
        ("runbooks", &["r"]),
        ("next_attention", &["a"]),
        ("toggle_sidebar", &["b"]),
        ("broadcast_toggle", &["y"]),
        ("select_for_broadcast", &["m"]),
        ("scroll_mode", &["[", "pageup", "v"]),
        ("search", &["/"]),
        ("open_preview", &["e"]),
        ("save_workspace", &["S"]),
        ("command_palette", &[":"]),
        ("detach", &["d", "q"]),
        ("help", &["?"]),
        ("send_prefix", &["ctrl+b"]),
    ],
    direct: &[
        ("focus_or_tab_left", &["alt+h", "alt+left"]),
        ("focus_or_tab_right", &["alt+l", "alt+right"]),
        ("focus_up", &["alt+k", "alt+up"]),
        ("focus_down", &["alt+j", "alt+down"]),
        ("new_pane", &["alt+n"]),
        ("toggle_floating", &["alt+f"]),
        ("next_attention", &["alt+a"]),
        ("workspace_picker", &["alt+o"]),
        ("agent_picker", &["alt+g"]),
    ],
    groups: &[],
    tool_menu: "",
};

pub const PRESETS: [&Preset; 2] = [&KRAAL, &CLASSIC];

/// `[keys]` of the configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct KeysConfig {
    /// "kraal" or "classic".
    pub preset: String,
    pub prefix: Vec<String>,
    /// Per action, replacing the preset's: sequences after the prefix.
    #[serde(default)]
    pub leader: BTreeMap<String, Vec<String>>,
    /// The earlier name of `leader`.
    #[serde(default)]
    pub prefixed: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub direct: BTreeMap<String, Vec<String>>,
}

/// What the keys typed after the leader lead to.
#[derive(Debug, PartialEq)]
pub enum Lookup {
    Run(Command),
    /// A menu: more keys to come.
    More,
    Nothing,
}

/// One line of the leader menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuEntry {
    pub key: KeySpec,
    pub label: String,
    /// It opens a further menu.
    pub group: bool,
    pub command: Option<Command>,
}

/// `text` without a leading `word` (any case) and the space after it, capitalised.
fn strip_word(text: &str, word: &str) -> Option<String> {
    let head = text.get(..word.len())?;
    let rest = text[word.len()..].strip_prefix(' ')?;
    let mut chars = rest.chars();
    let first = chars.next()?;
    head.eq_ignore_ascii_case(word).then(|| first.to_uppercase().chain(chars).collect())
}

/// Resolved key bindings.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    pub preset: &'static str,
    pub prefix: Vec<KeySpec>,
    pub direct: Vec<(KeySpec, Action)>,
    /// Sequences after the prefix, in menu order.
    pub leader: Vec<(Vec<KeySpec>, Command)>,
    groups: Vec<(KeySpec, &'static str)>,
    tool_menu: Vec<KeySpec>,
}

fn parse_sequence(text: &str) -> Result<Vec<KeySpec>, String> {
    let keys: Vec<KeySpec> = text
        .split_whitespace()
        .map(|k| KeySpec::parse(k).map_err(|e| format!("{k:?}: {e}")))
        .collect::<Result<_, _>>()?;
    if keys.is_empty() {
        return Err("empty".into());
    }
    Ok(keys)
}

/// Bindings of `table`, with the user's entries replacing the preset's for their actions.
fn bindings(
    preset: Table,
    user: &[&BTreeMap<String, Vec<String>>],
    ctx: &str,
    warnings: &mut Vec<String>,
) -> Vec<(Vec<KeySpec>, Action)> {
    let overridden: Vec<&str> = user.iter().flat_map(|m| m.keys()).map(String::as_str).collect();
    let mut out = Vec::new();
    let mut add = |name: &str, seqs: &[String], warnings: &mut Vec<String>| {
        let Some(action) = Action::parse(name) else {
            warnings.push(format!("{ctx}: unknown action {name:?}"));
            return;
        };
        for s in seqs {
            match parse_sequence(s) {
                Ok(keys) => out.push((keys, action)),
                Err(e) => warnings.push(format!("{ctx}.{name}: invalid key {e}")),
            }
        }
    };
    for (name, seqs) in preset.iter().filter(|(n, _)| !overridden.contains(n)) {
        let seqs: Vec<String> = seqs.iter().map(|s| s.to_string()).collect();
        add(name, &seqs, warnings);
    }
    for map in user {
        for (name, seqs) in *map {
            add(name, seqs, warnings);
        }
    }
    out
}

/// Whether two bindings would catch the same key press (`ctrl+m` and `enter` do).
fn same_key(a: &KeySpec, b: &KeySpec) -> bool {
    a.matches(&Key { code: b.code, mods: b.mods, raw: Vec::new() })
        || b.matches(&Key { code: a.code, mods: a.mods, raw: Vec::new() })
}

fn starts_with(seq: &[KeySpec], typed: &[Key]) -> bool {
    seq.len() >= typed.len() && seq.iter().zip(typed).all(|(s, k)| s.matches(k))
}

impl Keymap {
    /// Build from the configuration; also returns warnings about entries that were left out.
    pub fn build(cfg: &KeysConfig) -> (Keymap, Vec<String>) {
        let mut warnings = Vec::new();
        let preset = PRESETS.iter().find(|p| p.name == cfg.preset).copied().unwrap_or_else(|| {
            warnings.push(format!("keys.preset: no preset {:?} (kraal or classic)", cfg.preset));
            &KRAAL
        });
        let prefix: Vec<KeySpec> = cfg
            .prefix
            .iter()
            .filter_map(|s| {
                KeySpec::parse(s).map_err(|e| warnings.push(format!("keys.prefix: invalid key {s:?}: {e}"))).ok()
            })
            .collect();
        let direct: Vec<(KeySpec, Action)> = bindings(preset.direct, &[&cfg.direct], "keys.direct", &mut warnings)
            .into_iter()
            .filter_map(|(mut keys, a)| {
                if keys.len() > 1 {
                    warnings.push(format!("keys.direct.{}: one key, not a sequence", a.name()));
                    return None;
                }
                Some((keys.remove(0), a))
            })
            .collect();
        // A direct key that is also the prefix, or bound twice, would shadow the other.
        let mut kept: Vec<(KeySpec, Action)> = Vec::new();
        for (k, a) in direct {
            if prefix.iter().any(|p| same_key(p, &k)) {
                warnings.push(format!("keys.direct.{}: {k} is the prefix key", a.name()));
            } else if let Some((_, other)) = kept.iter().find(|(x, _)| same_key(x, &k)) {
                warnings.push(format!("keys.direct.{}: {k} is already {:?}", a.name(), other.describe()));
            } else {
                kept.push((k, a));
            }
        }
        let mut km = Keymap { preset: preset.name, prefix, direct: kept, ..Keymap::default() };
        for (keys, a) in bindings(preset.leader, &[&cfg.prefixed, &cfg.leader], "keys.leader", &mut warnings) {
            km.add(keys, Command::Action(a), &mut warnings);
        }
        km.groups = preset.groups.iter().filter_map(|(k, t)| Some((KeySpec::parse(k).ok()?, *t))).collect();
        km.tool_menu = parse_sequence(preset.tool_menu).unwrap_or_default();
        (km, warnings)
    }

    /// Bind a sequence, unless it collides with one already bound.
    fn add(&mut self, keys: Vec<KeySpec>, command: Command, warnings: &mut Vec<String>) -> bool {
        let shown = |k: &[KeySpec]| k.iter().map(KeySpec::to_string).collect::<Vec<_>>().join(" ");
        let what = |c: &Command| match c {
            Command::Action(a) => a.describe(),
            Command::Tool(t) => format!("tool {t}"),
        };
        for (other, c) in &self.leader {
            let n = other.len().min(keys.len());
            if other[..n].iter().zip(&keys[..n]).all(|(a, b)| same_key(a, b)) {
                warnings.push(if other.len() == keys.len() {
                    format!("key {:?} is already {:?}", shown(&keys), what(c))
                } else {
                    format!("key {:?} clashes with {:?} ({})", shown(&keys), shown(other), what(c))
                });
                return false;
            }
        }
        self.leader.push((keys, command));
        true
    }

    /// Bind the `key` of each tool (after the preset's tool menu); warnings for invalid or taken
    /// keys.
    pub fn bind_tools(&mut self, tools: &BTreeMap<String, crate::tools::ToolDef>) -> Vec<String> {
        let mut warnings = Vec::new();
        for (name, t) in tools.iter().filter(|(_, t)| t.enabled) {
            let Some(key) = &t.key else { continue };
            match KeySpec::parse(key) {
                Ok(k) => {
                    let mut keys = self.tool_menu.clone();
                    keys.push(k);
                    let mut why = Vec::new();
                    if !self.add(keys, Command::Tool(name.clone()), &mut why) {
                        warnings.extend(why.into_iter().map(|w| format!("tools.{name}: {w}")));
                    }
                }
                Err(e) => warnings.push(format!("tools.{name}: invalid key {key:?}: {e}")),
            }
        }
        warnings
    }

    pub fn is_prefix(&self, key: &Key) -> bool {
        self.prefix.iter().any(|p| p.matches(key))
    }

    pub fn direct(&self, key: &Key) -> Option<Action> {
        self.direct.iter().find(|(k, _)| k.matches(key)).map(|(_, a)| *a)
    }

    /// What the keys typed after the leader so far lead to.
    pub fn lookup(&self, typed: &[Key]) -> Lookup {
        let mut more = false;
        for (seq, cmd) in &self.leader {
            if !starts_with(seq, typed) {
                continue;
            }
            if seq.len() == typed.len() {
                return Lookup::Run(cmd.clone());
            }
            more = true;
        }
        if more { Lookup::More } else { Lookup::Nothing }
    }

    /// The keys that can follow `typed`, for the menu: commands and further menus.
    pub fn menu(&self, typed: &[Key]) -> Vec<MenuEntry> {
        let typed_specs: Vec<KeySpec> = typed.iter().map(|k| KeySpec::new(k.code, k.mods)).collect();
        let within = self.group_title(&typed_specs);
        let mut out: Vec<MenuEntry> = Vec::new();
        for (seq, cmd) in &self.leader {
            if seq.len() <= typed.len() || !starts_with(seq, typed) {
                continue;
            }
            let key = seq[typed.len()].clone();
            let group = seq.len() > typed.len() + 1;
            // One line per key, and per command (its first key).
            if out.iter().any(|e| e.key == key || (!group && e.command.as_ref() == Some(cmd))) {
                continue;
            }
            let label = if group {
                self.group_title(&seq[..=typed.len()]).unwrap_or("…").to_string()
            } else {
                let label = match cmd {
                    Command::Action(a) => a.label(),
                    Command::Tool(t) => t.clone(),
                };
                // The menu's name is not repeated: in Broadcast, "Broadcast to this tab" is "To this tab".
                match within.and_then(|title| strip_word(&label, title)) {
                    Some(rest) => rest,
                    None => label,
                }
            };
            out.push(MenuEntry { key, label, group, command: (!group).then(|| cmd.clone()) });
        }
        // The menus first (they are the map of what kraal does), then the commands.
        out.sort_by_key(|e| !e.group);
        out
    }

    /// The title of the menu `keys` open, if it has one.
    pub fn group_title(&self, keys: &[KeySpec]) -> Option<&'static str> {
        match keys {
            [k] => self.groups.iter().find(|(g, _)| g == k).map(|(_, t)| *t),
            _ => None,
        }
    }

    /// `ctrl+b p r`, or the direct key: how to reach an action, for hints and help.
    pub fn hint(&self, action: Action) -> Option<String> {
        if let Some((k, _)) = self.direct.iter().find(|(_, a)| *a == action) {
            return Some(k.to_string());
        }
        self.leader_hint(&Command::Action(action))
    }

    /// The leader sequence of a command, with the prefix.
    pub fn leader_hint(&self, command: &Command) -> Option<String> {
        let (seq, _) = self.leader.iter().find(|(_, c)| c == command)?;
        let mut parts = vec![self.prefix_label()];
        parts.extend(seq.iter().map(KeySpec::to_string));
        Some(parts.join(" "))
    }

    /// Every way to reach `command`: direct keys, then leader sequences.
    pub fn all_hints(&self, command: &Command) -> Vec<String> {
        let mut out: Vec<String> = match command {
            Command::Action(a) => self.direct.iter().filter(|(_, x)| x == a).map(|(k, _)| k.to_string()).collect(),
            Command::Tool(_) => Vec::new(),
        };
        let prefix = self.prefix_label();
        out.extend(
            self.leader.iter().filter(|(_, c)| c == command).map(|(seq, _)| {
                format!("{prefix} {}", seq.iter().map(KeySpec::to_string).collect::<Vec<_>>().join(" "))
            }),
        );
        out
    }

    pub fn prefix_label(&self) -> String {
        self.prefix.first().map(|k| k.to_string()).unwrap_or_else(|| "prefix".into())
    }
}

/// A key as menus show it: arrows as arrows (unless `ascii`).
pub fn pretty(key: &KeySpec, ascii: bool) -> String {
    if !ascii && key.mods == Default::default() {
        let arrow = match key.code {
            KeyCode::Left => Some("←"),
            KeyCode::Right => Some("→"),
            KeyCode::Up => Some("↑"),
            KeyCode::Down => Some("↓"),
            KeyCode::Enter => Some("⏎"),
            _ => None,
        };
        if let Some(a) = arrow {
            return a.to_string();
        }
    }
    key.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    fn cfg(user: &str) -> crate::config::Config {
        crate::config::Config::from_user_toml(user).unwrap()
    }

    fn key(c: char) -> Key {
        Key { code: KeyCode::Char(c), mods: Mods::NONE, raw: vec![c as u8] }
    }

    #[test]
    fn presets_bind_cleanly() {
        for preset in PRESETS {
            let (km, warnings) = Keymap::build(&cfg(&format!("[keys]\npreset = \"{}\"\n", preset.name)).keys);
            assert!(warnings.is_empty(), "{}: {warnings:?}", preset.name);
            assert!(km.leader.iter().any(|(_, c)| *c == Command::Action(Tab(9))));
            assert!(km.leader.iter().any(|(_, c)| *c == Command::Action(Help)));
            assert!(km.direct.iter().any(|(_, a)| *a == NextAttention));
        }
        for (a, name, _) in ACTIONS {
            assert_eq!(Action::parse(name), Some(*a));
        }
    }

    #[test]
    fn sequences_menus_and_hints() {
        let (km, _) = Keymap::build(&cfg("").keys);
        assert_eq!(km.preset, "kraal");
        assert_eq!(km.lookup(&[key('p')]), Lookup::More);
        let right = Key { code: KeyCode::Right, mods: Mods::NONE, raw: Vec::new() };
        assert_eq!(km.lookup(&[key('p'), right]), Lookup::Run(Command::Action(SplitRight)));
        assert_eq!(km.lookup(&[key('n')]), Lookup::Run(Command::Action(NewPane)));
        assert_eq!(km.lookup(&[key('q')]), Lookup::Nothing);
        let root = km.menu(&[]);
        let panes = root.iter().find(|e| e.key == KeySpec::parse("p").unwrap()).unwrap();
        assert!(panes.group && panes.label == "Panes");
        assert!(root.iter().any(|e| e.label == "New pane" && !e.group));
        let p = km.menu(&[key('p')]);
        assert!(p.iter().any(|e| e.label == "Resize panes"));
        let b: Vec<String> = km.menu(&[key('b')]).into_iter().map(|e| e.label).collect();
        assert!(b.contains(&"To this tab".to_string()) && b.contains(&"Stop broadcasting".to_string()), "{b:?}");
        assert_eq!(km.hint(SplitRight).as_deref(), Some("ctrl+b p right"));
        assert_eq!(km.hint(NewPane).as_deref(), Some("alt+n"));
    }

    #[test]
    fn user_bindings_replace_the_preset_and_collisions_are_reported() {
        let (km, warnings) = Keymap::build(&cfg("[keys.leader]\nsplit_right = [\"%\"]\nnew_tab = [\"p\"]\n").keys);
        assert_eq!(km.lookup(&[key('%')]), Lookup::Run(Command::Action(SplitRight)));
        assert!(km.all_hints(&Command::Action(SplitRight)).iter().all(|h| !h.contains("right")));
        assert_eq!(warnings.len(), 1, "p is the Panes menu: {warnings:?}");
        // The older table name still works.
        let (km, warnings) = Keymap::build(&cfg("[keys.prefixed]\ntoggle_sync = [\"y\"]\n").keys);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(km.lookup(&[key('y')]), Lookup::Run(Command::Action(BroadcastToggle)));
        let (_, warnings) = Keymap::build(&cfg("[keys]\npreset = \"emacs\"\n").keys);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn tools_go_in_the_run_menu() {
        let c = cfg("[tools.lazygit]\nkey = \"l\"\n[tools.mine]\ncommand = \"tig\"\nkey = \"g\"\n");
        let (mut km, _) = Keymap::build(&c.keys);
        let warnings = km.bind_tools(&c.tools);
        assert_eq!(warnings.len(), 1, "r g is git: {warnings:?}");
        assert_eq!(km.lookup(&[key('r'), key('l')]), Lookup::Run(Command::Tool("lazygit".into())));
        // Classic: a tool key follows the prefix directly, and may not take a command's key.
        let classic_cfg = cfg(
            "[keys]\npreset = \"classic\"\n[tools.lazygit]\nkey = \"i\"\n[tools.mine]\ncommand = \"tig\"\nkey = \"l\"\n",
        );
        let (mut classic, _) = Keymap::build(&classic_cfg.keys);
        let warnings = classic.bind_tools(&classic_cfg.tools);
        assert!(warnings.len() == 1 && warnings[0].contains("mine"), "l is focus right: {warnings:?}");
        assert_eq!(classic.lookup(&[key('i')]), Lookup::Run(Command::Tool("lazygit".into())));
        assert_eq!(classic.lookup(&[key('l')]), Lookup::Run(Command::Action(FocusRight)));
        // Keys that send the same byte collide (ctrl+m is enter).
        let c = cfg("[tools.mine]\ncommand = \"tig\"\nkey = \"ctrl+m\"\n");
        let (mut km, _) = Keymap::build(&c.keys);
        assert_eq!(km.bind_tools(&c.tools).len(), 1, "r ctrl+m is r enter");
        let (_, warnings) = Keymap::build(&cfg("[keys.direct]\nnew_tab = [\"ctrl+b\"]\n").keys);
        assert!(warnings.iter().any(|w| w.contains("prefix")), "{warnings:?}");
    }
}
