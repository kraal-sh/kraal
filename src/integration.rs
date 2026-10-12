//! Agent hooks: `kraal hook <agent>` reports state changes to the server, and
//! `kraal integration install <agent>` wires that command into the agent's configuration: its
//! hooks, its notify command, or a plugin of kraal's own where the agent loads plugins. For
//! shells, `kraal integration install <shell>` adds the lines that mark prompts and commands
//! (OSC 133) to the shell's startup file.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::agent::{AgentState, hook_state};
use crate::client;
use crate::protocol::Request;

/// The agents whose hooks kraal installs.
pub const AGENTS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "qwen",
    "droid",
    "codebuddy",
    "auggie",
    "cursor",
    "copilot",
    "kimi",
    "opencode",
    "kilo",
    "pi",
];

/// Hooks laid out as Claude Code lays them out, in a JSON file: each event holds a list of
/// groups, `{ "matcher": …, "hooks": [{ "type": "command", "command": …, "timeout": … }] }`.
struct NestedHooks {
    /// The agent, as `kraal hook <agent>` names it.
    agent: &'static str,
    /// The events kraal listens to, with the matcher tool events take (every tool).
    events: &'static [(&'static str, Option<&'static str>)],
    /// In the agent's unit: seconds, or milliseconds for Gemini CLI and Augment.
    timeout: u64,
}

/// Claude Code's events kraal listens to.
const CLAUDE_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PreToolUse", Some("*")),
    ("PostToolUse", Some("*")),
    ("Notification", None),
    ("Stop", None),
    ("SessionEnd", None),
];

/// The agents whose hooks are laid out as Claude Code's.
const NESTED: &[NestedHooks] = &[
    NestedHooks { agent: "claude", events: CLAUDE_EVENTS, timeout: 5 },
    // Factory's droid and Tencent's CodeBuddy Code take Claude Code's hooks as they are.
    NestedHooks { agent: "droid", events: CLAUDE_EVENTS, timeout: 5 },
    NestedHooks { agent: "codebuddy", events: CLAUDE_EVENTS, timeout: 5 },
    NestedHooks {
        agent: "gemini",
        events: &[
            ("SessionStart", None),
            ("BeforeAgent", None),
            ("BeforeTool", Some("*")),
            ("AfterTool", Some("*")),
            ("Notification", None),
            ("AfterAgent", None),
            ("SessionEnd", None),
        ],
        timeout: 5000,
    },
    NestedHooks {
        agent: "qwen",
        events: &[
            ("SessionStart", None),
            ("UserPromptSubmit", None),
            ("PreToolUse", Some("*")),
            ("PostToolUse", Some("*")),
            ("Notification", None),
            ("Stop", None),
            ("StopFailure", None),
            ("SessionEnd", None),
        ],
        timeout: 5,
    },
    // Augment's tool hooks match every tool without a matcher.
    NestedHooks {
        agent: "auggie",
        events: &[
            ("SessionStart", None),
            ("PreToolUse", None),
            ("PostToolUse", None),
            ("Stop", None),
            ("SessionEnd", None),
        ],
        timeout: 5000,
    },
];

fn nested(agent: &str) -> &'static NestedHooks {
    NESTED.iter().find(|n| n.agent == agent).expect("an agent with nested hooks")
}

/// Cursor's hooks (`~/.cursor/hooks.json`): none of them asks for an answer kraal would have to
/// give, unlike its permission hooks.
const CURSOR_EVENTS: &[&str] = &["sessionStart", "beforeSubmitPrompt", "postToolUse", "stop", "sessionEnd"];

/// Copilot CLI's events, with the matcher (on the notification's type) of those that take one.
const COPILOT_EVENTS: &[(&str, Option<&str>)] = &[
    ("sessionStart", None),
    ("userPromptSubmitted", None),
    ("postToolUse", None),
    ("notification", Some("permission_prompt|elicitation_dialog")),
    ("agentStop", None),
    ("sessionEnd", None),
];

/// Kimi Code's events: approval requests and Esc have their own.
const KIMI_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PermissionResult",
    "Stop",
    "StopFailure",
    "Interrupt",
    "SessionEnd",
];

/// Agents whose hooks pass a JSON payload on stdin.
const JSON_PAYLOADS: &[&str] =
    &["claude", "gemini", "qwen", "droid", "codebuddy", "auggie", "cursor", "copilot", "kimi"];

/// Entry point of `kraal hook <agent>`. Never fails loudly and never prints: agents treat hook
/// output and exit codes as instructions.
pub fn run_hook(agent: &str, arg: Option<String>) {
    let (Some(pane), Some(socket)) = (std::env::var_os("KRAAL_PANE"), std::env::var_os("KRAAL_SOCKET")) else {
        return; // not running inside kraal
    };
    let pane = pane.to_string_lossy().into_owned();
    let socket = PathBuf::from(socket);
    let report = match agent {
        a if JSON_PAYLOADS.contains(&a) => {
            let mut input = String::new();
            let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
            let Ok(payload) = serde_json::from_str::<Value>(&input) else { return };
            // Cursor runs Claude Code's hooks too, with payloads of its own.
            if a == "claude" && payload.get("cursor_version").is_some() {
                return;
            }
            // Copilot's hooks name their event on kraal's command line.
            let state = hook_state(a, &payload, arg.as_deref());
            let field = |k: &str| payload.get(k).and_then(Value::as_str).map(String::from);
            // Cursor and Augment name it a conversation.
            let session_id = field("session_id").or_else(|| field("sessionId")).or_else(|| field("conversation_id"));
            let message = field("message");
            if state.is_none() && session_id.is_none() {
                return;
            }
            Request::AgentReport { pane, agent: a.into(), state, session_id, message }
        }
        "codex" => {
            // Codex passes its notification as the last argument.
            let Some(arg) = arg else { return };
            let Ok(payload) = serde_json::from_str::<Value>(&arg) else { return };
            let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
            let state = match kind {
                "agent-turn-complete" => Some(AgentState::Done),
                _ => None,
            };
            let session_id = payload.get("thread-id").and_then(Value::as_str).map(String::from);
            Request::AgentReport { pane, agent: "codex".into(), state, session_id, message: None }
        }
        other => {
            // Generic: `kraal hook <agent> <state>` from any script.
            let state = arg.as_deref().and_then(AgentState::parse);
            Request::AgentReport { pane, agent: other.into(), state, session_id: None, message: None }
        }
    };
    let _ = client::request_quick(&socket, report);
}

fn claude_settings_path() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
        .join("settings.json")
}

fn codex_config_path() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
        .join("config.toml")
}

/// An agent's own directory: kraal's `all` covers the agents whose directory exists.
fn agent_dir(agent: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let config = var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    match agent {
        "claude" => claude_settings_path().parent().map(Path::to_path_buf).unwrap_or_default(),
        "codex" => codex_config_path().parent().map(Path::to_path_buf).unwrap_or_default(),
        "gemini" => var("GEMINI_CLI_HOME").unwrap_or(home).join(".gemini"),
        "qwen" => home.join(".qwen"),
        "droid" => home.join(".factory"),
        "codebuddy" => home.join(".codebuddy"),
        "auggie" => home.join(".augment"),
        "cursor" => home.join(".cursor"),
        "copilot" => var("COPILOT_HOME").unwrap_or_else(|| home.join(".copilot")),
        "kimi" => var("KIMI_CODE_HOME").unwrap_or_else(|| home.join(".kimi-code")),
        "opencode" => config.join("opencode"),
        "kilo" => config.join("kilo"),
        // pi
        _ => var("PI_CODING_AGENT_DIR").unwrap_or_else(|| home.join(".pi/agent")),
    }
}

/// The file kraal changes for an agent, and for nested hooks whether its events are the
/// file's top level (rather than its `hooks` key).
fn agent_file(agent: &str) -> (PathBuf, bool) {
    let dir = agent_dir(agent);
    let file = match agent {
        "claude" => return (claude_settings_path(), false),
        "codex" => return (codex_config_path(), false),
        // Droid reads `hooks.json`, or the `hooks` of its settings when there is no such file.
        "droid" => {
            let settings = dir.join("settings.json");
            if !dir.join("hooks.json").exists() && read_json(&settings).is_ok_and(|v| v.get("hooks").is_some()) {
                return (settings, false);
            }
            return (dir.join("hooks.json"), true);
        }
        "cursor" => "hooks.json",
        "copilot" => "hooks/kraal.json",
        "kimi" => "config.toml",
        "opencode" => "plugins/kraal.js",
        "kilo" => "plugin/kraal.js",
        "pi" => "extensions/kraal.ts",
        // Gemini CLI, Qwen Code, CodeBuddy, Augment
        _ => "settings.json",
    };
    (dir.join(file), false)
}

/// The agents `all` covers: Claude Code and Codex, and every other one used on this machine.
pub fn agents_here() -> Vec<&'static str> {
    AGENTS.iter().copied().filter(|a| matches!(*a, "claude" | "codex") || agent_dir(a).is_dir()).collect()
}

fn unknown(agent: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown agent or shell {agent:?} (supported: {}, {})", AGENTS.join(", "), SHELLS.join(", "))
}

/// The command the agent's hooks run: a shell parses it, so the path is quoted whatever it holds.
fn hook_command(agent: &str) -> Result<String> {
    let exe = std::env::current_exe()?;
    Ok(format!("{} hook {agent}", crate::shell::quote(&exe.to_string_lossy())))
}

/// Augment runs a script by its path, not a command line: the one kraal puts in its directory.
const AUGGIE_SCRIPT: &str = "kraal-hook.sh";

/// Whether a hook's command is kraal's: as this kraal writes it, or as kraal 0.2.0 wrote
/// Augment's (`<kraal> hook auggie`), so that installing again replaces it and uninstalling
/// removes it.
fn is_ours(cmd: &str, agent: &str) -> bool {
    (cmd.contains("kraal") && cmd.ends_with(&format!(" hook {agent}")))
        || (agent == "auggie" && cmd.ends_with(&format!("/{AUGGIE_SCRIPT}")))
}

/// Whether a hook's command is kraal's as this kraal writes it. Augment does not run the command
/// line kraal 0.2.0 gave its hooks: those count as not installed. Once 0.2.0 is no longer
/// supported (docs/compatibility.md), [`is_ours`] can stop taking them, and this check becomes
/// that one.
fn is_current(cmd: &str, agent: &str) -> bool {
    is_ours(cmd, agent) && (agent != "auggie" || cmd.ends_with(&format!("/{AUGGIE_SCRIPT}")))
}

/// `kraal integration install <agent or shell>`. Returns what was done, for the user.
pub fn install(agent: &str) -> Result<String> {
    if let Some(s) = SHELLS.iter().find(|s| **s == agent) {
        return install_shell(s, &shell_rc_path(s));
    }
    if !AGENTS.contains(&agent) {
        return Err(unknown(agent));
    }
    let (path, top_level) = agent_file(agent);
    match agent {
        "claude" => install_claude(&path, &crate::accounts::Accounts::path()),
        "codex" => install_codex(&path),
        "cursor" => install_cursor(&path),
        "kimi" => install_kimi(&path),
        "copilot" => install_own(agent, &path, &own_file(agent)?, "hooks"),
        "opencode" | "kilo" => install_own(agent, &path, &own_file(agent)?, "plugin"),
        "pi" => install_own(agent, &path, &own_file(agent)?, "extension"),
        "auggie" => install_auggie(&path, &agent_dir(agent).join(AUGGIE_SCRIPT), &own_file(agent)?),
        _ => install_nested(nested(agent), &path, top_level, &hook_command(agent)?),
    }
}

/// What kraal writes in the file of its own it puts in an agent's directory.
fn own_file(agent: &str) -> Result<String> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    Ok(match agent {
        "copilot" => copilot_hooks(&exe),
        "pi" => pi_extension(&exe),
        "auggie" => auggie_script(&exe),
        _ => opencode_plugin(agent, &exe),
    })
}

/// `kraal integration uninstall <agent or shell>`: takes out only what kraal put in. Returns
/// what was done, for the user.
pub fn uninstall(agent: &str) -> Result<String> {
    if let Some(s) = SHELLS.iter().find(|s| **s == agent) {
        return uninstall_shell(s, &shell_rc_path(s));
    }
    if !AGENTS.contains(&agent) {
        return Err(unknown(agent));
    }
    let (path, top_level) = agent_file(agent);
    match agent {
        "claude" => uninstall_claude(&path, &crate::accounts::Accounts::path()),
        "codex" => uninstall_codex(&path),
        "cursor" => uninstall_cursor(&path),
        "kimi" => uninstall_kimi(&path),
        "copilot" | "opencode" | "kilo" | "pi" => uninstall_own(agent, &path, &own_file(agent)?),
        "auggie" => uninstall_auggie(&path, &agent_dir(agent).join(AUGGIE_SCRIPT), &own_file(agent)?),
        _ => uninstall_nested(nested(agent), &path, top_level),
    }
}

/// What `install` adds to a shell's startup file, for those who keep it themselves.
pub fn show(shell: &str) -> Result<String> {
    if !SHELLS.contains(&shell) {
        bail!("nothing to show for {shell:?}: zsh, bash or fish");
    }
    Ok(shell_block(shell).trim_end().to_string())
}

/// Whether kraal's hooks or shell lines are in place for `agent`, as a line for the user.
pub fn status(agent: &str) -> Result<String> {
    if let Some(s) = SHELLS.iter().find(|s| **s == agent) {
        return Ok(shell_status(s, &shell_rc_path(s)));
    }
    if !AGENTS.contains(&agent) {
        return Err(unknown(agent));
    }
    let (path, top_level) = agent_file(agent);
    status_line(agent, &path, top_level)
}

fn status_line(agent: &str, path: &Path, top_level: bool) -> Result<String> {
    let (installed, wanted): (Vec<&str>, Vec<&str>) = match agent {
        "codex" => {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let ours = text.lines().any(|l| l.trim_start().starts_with("notify") && l.contains("kraal"));
            (if ours { vec!["notify"] } else { vec![] }, vec!["notify"])
        }
        "copilot" | "opencode" | "kilo" | "pi" => {
            let ours = std::fs::read_to_string(path).is_ok_and(|t| is_own(&t, agent));
            (if ours { vec!["plugin"] } else { vec![] }, vec!["plugin"])
        }
        "cursor" => {
            let v = read_json(path)?;
            let events = v.get("hooks");
            (CURSOR_EVENTS.iter().copied().filter(|e| flat_has(events, e, "cursor")).collect(), CURSOR_EVENTS.to_vec())
        }
        "kimi" => (kimi_events(path)?, KIMI_EVENTS.to_vec()),
        _ => {
            let n = nested(agent);
            let v = read_json(path)?;
            let events = if top_level { Some(&v) } else { v.get("hooks") };
            let all: Vec<&str> = n.events.iter().map(|(e, _)| *e).collect();
            (all.iter().copied().filter(|e| nested_has(events, e, agent)).collect(), all)
        }
    };
    Ok(match installed.len() {
        0 => format!("{agent}: not installed ({})", path.display()),
        n if n == wanted.len() => format!("{agent}: installed ({})", path.display()),
        _ => {
            format!("{agent}: partially installed ({}); run `kraal integration install {agent}`", installed.join(", "))
        }
    })
}

/// A JSON file; `{}` when it is missing or blank.
fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(json!({})),
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Replace a file of the user's (an agent's settings, a shell's startup file) atomically, after
/// copying what it held to `<name>.kraal-bak`.
fn write_with_backup(path: &Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let backup = path.with_file_name(format!("{name}.kraal-bak"));
        std::fs::copy(path, &backup).with_context(|| format!("backing up {}", path.display()))?;
    }
    // A symlink (dotfiles kept in a repository) stays one: the file it points to changes.
    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path)
            .with_context(|| format!("{} is a link to a file that is not there", path.display()))?,
        _ => path.to_path_buf(),
    };
    crate::paths::write_atomic(&target, content.as_bytes()).with_context(|| format!("writing {}", target.display()))
}

fn runs_ours(hook: &Value, agent: &str) -> bool {
    hook.get("command").and_then(Value::as_str).is_some_and(|c| is_ours(c, agent))
}

/// Whether kraal's hook, as this kraal writes it, is in `event`'s groups of an event map laid
/// out as Claude Code's.
fn nested_has(events: Option<&Value>, event: &str, agent: &str) -> bool {
    let current = |h: &Value| h.get("command").and_then(Value::as_str).is_some_and(|c| is_current(c, agent));
    events.and_then(|e| e.get(event)).and_then(Value::as_array).is_some_and(|groups| {
        groups.iter().any(|g| g.get("hooks").and_then(Value::as_array).is_some_and(|hs| hs.iter().any(current)))
    })
}

/// Put kraal's hook in each event of `spec`, once: stale entries (an old binary path) go first.
/// The user's own hooks stay where they are.
fn add_nested(events: &mut Map<String, Value>, spec: &NestedHooks, cmd: &str) -> Result<usize> {
    let mut added = 0;
    for (event, matcher) in spec.events {
        let groups = events.entry(event.to_string()).or_insert_with(|| json!([]));
        let groups = groups.as_array_mut().with_context(|| format!("hooks.{event} is not an array"))?;
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                hs.retain(|h| !runs_ours(h, spec.agent));
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|hs| !hs.is_empty()));
        let mut group = json!({ "hooks": [{ "type": "command", "command": cmd, "timeout": spec.timeout }] });
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        groups.push(group);
        added += 1;
    }
    Ok(added)
}

/// Take kraal's hooks out of an event map, and the groups and events left empty.
fn remove_nested(events: &mut Map<String, Value>, agent: &str) -> usize {
    let mut removed = 0;
    for (_, groups) in events.iter_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = hs.len();
                hs.retain(|h| !runs_ours(h, agent));
                removed += before - hs.len();
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|hs| !hs.is_empty()));
    }
    events.retain(|_, groups| groups.as_array().is_none_or(|a| !a.is_empty()));
    removed
}

/// The event map of a settings file: its `hooks` key, or the file itself.
fn event_map<'a>(v: &'a mut Value, path: &Path, top_level: bool) -> Result<&'a mut Map<String, Value>> {
    let root = v.as_object_mut().with_context(|| format!("{} is not a JSON object", path.display()))?;
    if top_level {
        return Ok(root);
    }
    root.entry("hooks").or_insert_with(|| json!({})).as_object_mut().context("\"hooks\" is not an object")
}

fn install_nested(spec: &NestedHooks, path: &Path, top_level: bool, cmd: &str) -> Result<String> {
    let mut v = read_json(path)?;
    let added = add_nested(event_map(&mut v, path, top_level)?, spec, cmd)?;
    write_with_backup(path, &(serde_json::to_string_pretty(&v)? + "\n"))?;
    Ok(format!("{}: {added} hooks installed in {} (backup next to it)", spec.agent, path.display()))
}

/// Augment's hooks run kraal's script, which runs `kraal hook auggie`: the script first, then
/// the hooks that run it.
fn install_auggie(settings: &Path, script: &Path, content: &str) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    install_own("auggie", script, content, "hook script")?;
    std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", script.display()))?;
    install_nested(nested("auggie"), settings, false, &script.to_string_lossy())
}

/// The hooks first, then the script they ran.
fn uninstall_auggie(settings: &Path, script: &Path, content: &str) -> Result<String> {
    let hooks = uninstall_nested(nested("auggie"), settings, false)?;
    let own = uninstall_own("auggie", script, content)?;
    Ok(if own.ends_with("nothing to remove") { hooks } else { format!("{hooks}; {own}") })
}

/// The script Augment's hooks run.
fn auggie_script(exe: &str) -> String {
    format!(
        "#!/bin/sh\n# kraal: tells the kraal pane Augment runs in whether it works or is done.\n\
         # `kraal integration uninstall auggie` removes this file and the hooks that run it.\nexec {} hook auggie\n",
        crate::shell::quote(exe)
    )
}

fn uninstall_nested(spec: &NestedHooks, path: &Path, top_level: bool) -> Result<String> {
    let mut v = read_json(path)?;
    let removed = match if top_level { v.as_object_mut() } else { v.get_mut("hooks").and_then(Value::as_object_mut) } {
        Some(events) => remove_nested(events, spec.agent),
        None => 0,
    };
    if removed == 0 {
        return Ok(format!("{}: nothing to remove", spec.agent));
    }
    write_with_backup(path, &(serde_json::to_string_pretty(&v)? + "\n"))?;
    Ok(format!("{}: removed {removed} hooks from {}", spec.agent, path.display()))
}

fn is_our_statusline(cmd: &str) -> bool {
    cmd.contains("kraal") && cmd.trim_end().ends_with(" statusline")
}

fn install_claude(path: &Path, accounts_path: &Path) -> Result<String> {
    let mut v = read_json(path)?;
    let cmd = hook_command("claude")?;
    let root = v.as_object_mut().context("settings.json is not a JSON object")?;
    // The status line reports each account's usage; the user's own one keeps running after it.
    let current = root.get("statusLine").and_then(|s| s.get("command")).and_then(Value::as_str).map(String::from);
    if !current.as_deref().is_some_and(is_our_statusline) {
        // Under the accounts lock: another kraal may be adding an account meanwhile.
        crate::accounts::Accounts::update_at(accounts_path, |acc| {
            acc.claude.statusline = current;
            Ok(())
        })?;
    }
    let ours = cmd.trim_end_matches(" hook claude").to_string() + " statusline";
    let mut line = root.get("statusLine").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
    line["type"] = json!("command");
    line["command"] = json!(ours);
    root.insert("statusLine".into(), line);
    let added = add_nested(event_map(&mut v, path, false)?, nested("claude"), &cmd)?;
    write_with_backup(path, &(serde_json::to_string_pretty(&v)? + "\n"))?;
    Ok(format!("claude: {added} hooks installed in {} (backup next to it)", path.display()))
}

fn uninstall_claude(path: &Path, accounts_path: &Path) -> Result<String> {
    let mut v = read_json(path)?;
    let mut removed = 0;
    // Put the user's own status line back.
    if let Some(root) = v.as_object_mut()
        && root.get("statusLine").and_then(|s| s.get("command")).and_then(Value::as_str).is_some_and(is_our_statusline)
    {
        let original = crate::accounts::Accounts::load_from(accounts_path).ok().and_then(|a| a.claude.statusline);
        match original {
            Some(cmd) => root["statusLine"]["command"] = json!(cmd),
            None => {
                root.remove("statusLine");
            }
        }
        removed += 1;
    }
    if let Some(hooks) = v.get_mut("hooks").and_then(Value::as_object_mut) {
        removed += remove_nested(hooks, "claude");
    }
    if removed == 0 {
        return Ok("claude: nothing to remove".into());
    }
    write_with_backup(path, &(serde_json::to_string_pretty(&v)? + "\n"))?;
    Ok(format!("claude: removed {removed} hooks from {}", path.display()))
}

/// Whether kraal's hook is in an event of Cursor's hooks, a plain list of commands.
fn flat_has(events: Option<&Value>, event: &str, agent: &str) -> bool {
    events.and_then(|e| e.get(event)).and_then(Value::as_array).is_some_and(|hs| hs.iter().any(|h| runs_ours(h, agent)))
}

fn install_cursor(path: &Path) -> Result<String> {
    let mut v = read_json(path)?;
    let cmd = hook_command("cursor")?;
    let root = v.as_object_mut().with_context(|| format!("{} is not a JSON object", path.display()))?;
    root.entry("version").or_insert(json!(1));
    let events = root.entry("hooks").or_insert_with(|| json!({}));
    let events = events.as_object_mut().context("\"hooks\" is not an object")?;
    for event in CURSOR_EVENTS {
        let hooks = events.entry(event.to_string()).or_insert_with(|| json!([]));
        let hooks = hooks.as_array_mut().with_context(|| format!("hooks.{event} is not an array"))?;
        hooks.retain(|h| !runs_ours(h, "cursor"));
        hooks.push(json!({ "command": cmd }));
    }
    write_with_backup(path, &(serde_json::to_string_pretty(&v)? + "\n"))?;
    Ok(format!("cursor: {} hooks installed in {} (backup next to it)", CURSOR_EVENTS.len(), path.display()))
}

fn uninstall_cursor(path: &Path) -> Result<String> {
    let mut v = read_json(path)?;
    let mut removed = 0;
    if let Some(events) = v.get_mut("hooks").and_then(Value::as_object_mut) {
        for (_, hooks) in events.iter_mut() {
            if let Some(hooks) = hooks.as_array_mut() {
                let before = hooks.len();
                hooks.retain(|h| !runs_ours(h, "cursor"));
                removed += before - hooks.len();
            }
        }
        events.retain(|_, hooks| hooks.as_array().is_none_or(|a| !a.is_empty()));
    }
    if removed == 0 {
        return Ok("cursor: nothing to remove".into());
    }
    write_with_backup(path, &(serde_json::to_string_pretty(&v)? + "\n"))?;
    Ok(format!("cursor: removed {removed} hooks from {}", path.display()))
}

/// Whether a file kraal put in an agent's directory is still kraal's own.
fn is_own(text: &str, agent: &str) -> bool {
    text.contains("kraal")
        && (text.contains(&format!("kraal integration uninstall {agent}")) || text.contains(&format!(" hook {agent} ")))
}

/// What a file kraal puts in an agent's directory holds now: nothing (no such file), or text
/// that is kraal's own. A file kraal cannot read, or that is not kraal's, is refused: it may be
/// the user's.
fn own_text(agent: &str, path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        Ok(_) => {}
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}; kraal leaves it alone", path.display()))?;
    anyhow::ensure!(is_own(&text, agent), "{} is not kraal's; kraal leaves it alone", path.display());
    Ok(Some(text))
}

/// A file of kraal's own where the agent loads every file it finds (hooks, plugins,
/// extensions): written whole, removed whole. One kraal wrote before is copied next to it
/// (`.kraal-bak`) when it changes, in case it was edited.
fn install_own(agent: &str, path: &Path, content: &str, what: &str) -> Result<String> {
    match own_text(agent, path)? {
        Some(text) if text == content => {}
        Some(_) => write_with_backup(path, content)?,
        None => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            crate::paths::write_atomic(path, content.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(format!("{agent}: {what} installed in {}; it takes effect in new sessions", path.display()))
}

/// Remove a file of kraal's own; one that is not what this kraal writes (edited, or from another
/// version) is copied next to it first.
fn uninstall_own(agent: &str, path: &Path, content: &str) -> Result<String> {
    let Some(text) = own_text(agent, path)? else { return Ok(format!("{agent}: nothing to remove")) };
    if text != content {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        std::fs::write(path.with_file_name(format!("{name}.kraal-bak")), &text)
            .with_context(|| format!("backing up {}", path.display()))?;
    }
    std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    Ok(format!("{agent}: removed {}", path.display()))
}

/// Copilot CLI loads every `*.json` in its hooks directory. Its own format does not name the
/// event in the payload: kraal's command line does.
fn copilot_hooks(exe: &str) -> String {
    let cmd = format!("{} hook copilot", crate::shell::quote(exe));
    let hooks: Map<String, Value> = COPILOT_EVENTS
        .iter()
        .map(|(event, matcher)| {
            let mut hook = json!({ "type": "command", "bash": format!("{cmd} {event}"), "timeoutSec": 5 });
            if let Some(m) = matcher {
                hook["matcher"] = json!(m);
            }
            (event.to_string(), json!([hook]))
        })
        .collect();
    serde_json::to_string_pretty(&json!({ "version": 1, "hooks": hooks })).unwrap_or_default() + "\n"
}

/// A plugin for OpenCode 1 (and Kilo, its fork): working while a session is busy, blocked
/// while a permission or a question waits, done when every session is idle again. Subagents
/// run sessions of their own, hence the counting.
fn opencode_plugin(agent: &str, exe: &str) -> String {
    let name = if agent == "kilo" { "Kilo" } else { "OpenCode" };
    // Kilo registers a default export with an id; OpenCode 1 every exported function.
    let export = if agent == "kilo" { "export default { id: \"kraal\", server: KraalPlugin };\n" } else { "" };
    let declare = if agent == "kilo" { "const KraalPlugin" } else { "export const KraalPlugin" };
    format!(
        r#"// kraal: tells the kraal pane {name} runs in whether it works, waits for you or is done.
// `kraal integration uninstall {agent}` removes this file.
const kraal = {exe};

{declare} = async ({{ $ }}) => {{
  if (!process.env.KRAAL_PANE) return {{}};
  const busy = new Set();
  const asking = new Map();
  let last = "done";
  let queue = Promise.resolve();
  const report = () => {{
    const state = asking.size ? "blocked" : busy.size ? "working" : "done";
    if (state === last) return;
    last = state;
    queue = queue.then(() => $`${{kraal}} hook {agent} ${{state}}`.quiet().nothrow()).catch(() => {{}});
  }};
  const settle = (session) => {{
    busy.delete(session);
    for (const [id, s] of asking) if (s === session) asking.delete(id);
  }};
  return {{
    event: async ({{ event }}) => {{
      const p = event.properties ?? {{}};
      switch (event.type) {{
        case "session.status":
          if (p.status?.type === "idle") settle(p.sessionID);
          else busy.add(p.sessionID);
          break;
        case "session.idle":
          settle(p.sessionID);
          break;
        case "permission.asked":
        case "permission.updated":
        case "question.asked":
          asking.set(p.id, p.sessionID);
          break;
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
          asking.delete(p.requestID ?? p.permissionID);
          break;
        default:
          return;
      }}
      report();
    }},
  }};
}};
{export}"#,
        exe = serde_json::to_string(exe).unwrap_or_default(),
    )
}

/// An extension for pi: it asks nothing by itself, so a question is an extension's prompt.
fn pi_extension(exe: &str) -> String {
    format!(
        r#"// kraal: tells the kraal pane pi runs in whether it works, waits for you or is done.
// `kraal integration uninstall pi` removes this file.
const kraal = {exe};

export default function (pi: any) {{
  if (!process.env.KRAAL_PANE) return;
  let running = false;
  let queue: Promise<unknown> = Promise.resolve();
  const report = (state: string) => {{
    queue = queue.then(() => pi.exec(kraal, ["hook", "pi", state])).catch(() => {{}});
    return queue;
  }};
  pi.on("session_start", () => {{ report("idle"); }});
  pi.on("agent_start", () => {{ running = true; report("working"); }});
  pi.on("ui_prompt_start", () => {{ report("blocked"); }});
  pi.on("ui_prompt_end", () => {{ report(running ? "working" : "idle"); }});
  pi.on("agent_settled", () => {{ running = false; report("done"); }});
  pi.on("session_shutdown", (event: any) => (event?.reason === "quit" ? report("unknown") : undefined));
}}
"#,
        exe = serde_json::to_string(exe).unwrap_or_default(),
    )
}

/// Kimi Code's `[[hooks]]` tables, one per event: kraal's go at the end of the file, and only
/// they are ever taken out.
fn install_kimi(path: &Path) -> Result<String> {
    let text = read_text(path)?;
    let cmd = hook_command("kimi")?;
    let mut lines = without_toml_hooks(&text, "kimi");
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let mut wanted = Vec::new();
    for event in KIMI_EVENTS {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push("[[hooks]]".into());
        lines.push(format!("event = {}", toml_string(event)));
        lines.push(format!("command = {}", toml_string(&cmd)));
        lines.push("timeout = 5".into());
        let mut t = toml::Table::new();
        t.insert("event".into(), (*event).into());
        t.insert("command".into(), cmd.clone().into());
        t.insert("timeout".into(), 5.into());
        wanted.push(toml::Value::Table(t));
    }
    let manual = format!(
        "add a [[hooks]] table with `command = {}` for each of {} to {}",
        toml_string(&cmd),
        KIMI_EVENTS.join(", "),
        path.display()
    );
    write_toml_hooks(path, &text, &lines, "kimi", &wanted, &manual)?;
    Ok(format!("kimi: {} hooks installed in {} (backup next to it)", KIMI_EVENTS.len(), path.display()))
}

fn uninstall_kimi(path: &Path) -> Result<String> {
    let text = read_text(path)?;
    let mut lines = without_toml_hooks(&text, "kimi");
    if lines.join("\n") == text.lines().collect::<Vec<_>>().join("\n") {
        return Ok("kimi: nothing to remove".into());
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let manual = format!("remove the [[hooks]] tables that run kraal from {}", path.display());
    write_toml_hooks(path, &text, &lines, "kimi", &[], &manual)?;
    Ok(format!("kimi: hooks removed from {}", path.display()))
}

/// The events of the `[[hooks]]` tables that run kraal.
fn kimi_events(path: &Path) -> Result<Vec<&'static str>> {
    let mut t: toml::Table =
        toml::from_str(&read_text(path)?).with_context(|| format!("parsing {}", path.display()))?;
    let ours = take_toml_hooks(&mut t, "kimi");
    Ok(KIMI_EVENTS
        .iter()
        .copied()
        .filter(|e| ours.iter().any(|h| h.get("event").and_then(|v| v.as_str()) == Some(e)))
        .collect())
}

/// A file's text; empty when it is missing.
fn read_text(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `text` without the `[[hooks]]` tables that run kraal. A table ends where the next one
/// starts; the comments and blank lines just before that belong to what follows.
fn without_toml_hooks(text: &str, agent: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let header = |l: &str| l.split('#').next().unwrap_or("").replace(' ', "").trim().to_string();
    let runs_ours = |l: &str| {
        toml::from_str::<toml::Table>(l)
            .ok()
            .and_then(|t| t.get("command").and_then(|c| c.as_str()).map(|c| is_ours(c, agent)))
            .unwrap_or(false)
    };
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if header(lines[i]) == "[[hooks]]" {
            let end = (i + 1..lines.len()).find(|&j| lines[j].trim_start().starts_with('[')).unwrap_or(lines.len());
            let mut last = end;
            while last > i + 1 && (lines[last - 1].trim().is_empty() || lines[last - 1].trim_start().starts_with('#')) {
                last -= 1;
            }
            if lines[i + 1..last].iter().any(|l| runs_ours(l)) {
                if out.last().is_some_and(|l| l.trim().is_empty()) {
                    out.pop();
                }
                i = last;
                continue;
            }
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    out
}

/// Take the entries of the top-level `hooks` array that run kraal out of a TOML document.
fn take_toml_hooks(t: &mut toml::Table, agent: &str) -> Vec<toml::Value> {
    let Some(toml::Value::Array(hooks)) = t.get_mut("hooks") else { return Vec::new() };
    let (ours, rest): (Vec<_>, Vec<_>) = std::mem::take(hooks)
        .into_iter()
        .partition(|h| h.get("command").and_then(|c| c.as_str()).is_some_and(|c| is_ours(c, agent)));
    if rest.is_empty() {
        t.remove("hooks");
    } else {
        *hooks = rest;
    }
    ours
}

/// Write a TOML file back only if, read as TOML, kraal's hooks are now `wanted` and nothing
/// else changed: the editing goes line by line, and a layout it could misread is left as it is.
fn write_toml_hooks(
    path: &Path,
    original: &str,
    lines: &[String],
    agent: &str,
    wanted: &[toml::Value],
    manual: &str,
) -> Result<()> {
    let text = lines.join("\n") + "\n";
    let mut before: toml::Table = toml::from_str(original)
        .with_context(|| format!("{} is not valid TOML; kraal leaves it alone: {manual}", path.display()))?;
    take_toml_hooks(&mut before, agent);
    let only_ours = toml::from_str::<toml::Table>(&text).is_ok_and(|mut after| {
        let ours = take_toml_hooks(&mut after, agent);
        after == before && ours == wanted
    });
    anyhow::ensure!(only_ours, "kraal cannot edit {} safely; {manual}", path.display());
    write_with_backup(path, &text)
}

fn install_codex(path: &Path) -> Result<String> {
    let text = read_text(path)?;
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let line = format!("notify = [{}, \"hook\", \"codex\"]", toml_string(&exe));
    // `notify` is a top-level key: it must come before the first [table].
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let first_table = first_table(&lines);
    if let Some(span) = notify_span(&lines, first_table) {
        let current = lines[span.clone()].join("\n");
        if !current.contains("kraal") {
            bail!("codex already has a notify command ({}); kraal does not replace it", current.trim());
        }
        lines.splice(span, [line.clone()]);
    } else {
        lines.insert(first_table, line.clone());
        if first_table < lines.len() - 1 {
            lines.insert(first_table + 1, String::new());
        }
    }
    let wanted = toml::Value::Array(vec![exe.into(), "hook".into(), "codex".into()]);
    let manual = format!("add `{line}` to {}, before its first [table]", path.display());
    write_codex_config(path, &text, &lines, Some(wanted), &manual)?;
    Ok(format!("codex: notify hook installed in {}", path.display()))
}

fn uninstall_codex(path: &Path) -> Result<String> {
    let text = read_text(path)?;
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let first_table = first_table(&lines);
    match notify_span(&lines, first_table) {
        Some(span) if lines[span.clone()].iter().any(|l| l.contains("kraal")) => {
            lines.drain(span);
        }
        _ => return Ok("codex: nothing to remove".into()),
    }
    let manual = format!("remove the `notify = …` setting that runs kraal from {}", path.display());
    write_codex_config(path, &text, &lines, None, &manual)?;
    Ok(format!("codex: notify hook removed from {}", path.display()))
}

/// Index of the first `[table]` line of a TOML file (or its length).
fn first_table(lines: &[String]) -> usize {
    lines.iter().position(|l| l.trim_start().starts_with('[')).unwrap_or(lines.len())
}

/// The lines of the top-level `notify = …` statement, which may span several lines (an array
/// written one element per line).
fn notify_span(lines: &[String], first_table: usize) -> Option<std::ops::Range<usize>> {
    let start = lines[..first_table].iter().position(|l| {
        let l = l.trim_start();
        l.strip_prefix("notify").is_some_and(|rest| rest.trim_start().starts_with('='))
    })?;
    // Brackets outside strings and comments, until the array closes.
    let mut depth = 0i32;
    for (i, line) in lines.iter().enumerate().skip(start) {
        let mut quote: Option<char> = None;
        let mut escaped = false;
        for c in line.chars() {
            match quote {
                Some(q) => {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' && q == '"' {
                        escaped = true;
                    } else if c == q {
                        quote = None;
                    }
                }
                None => match c {
                    '"' | '\'' => quote = Some(c),
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    '#' => break,
                    _ => {}
                },
            }
        }
        if depth <= 0 {
            return Some(start..i + 1);
        }
    }
    Some(start..lines.len())
}

/// Write Codex's configuration back only if, read as TOML, the edit changed `notify` to
/// `wanted` and nothing else. The editing goes line by line; a layout it could misread (a
/// string over several lines, say) is left as it is, and the user is told what to change.
fn write_codex_config(
    path: &Path,
    original: &str,
    lines: &[String],
    wanted: Option<toml::Value>,
    manual: &str,
) -> Result<()> {
    let text = lines.join("\n") + "\n";
    let before: toml::Table = toml::from_str(original)
        .with_context(|| format!("{} is not valid TOML; kraal leaves it alone: {manual}", path.display()))?;
    let after: Option<toml::Table> = toml::from_str(&text).ok();
    let only_notify = after.is_some_and(|mut after| {
        let notify = after.remove("notify");
        let mut rest = before.clone();
        rest.remove("notify");
        after == rest && notify == wanted
    });
    anyhow::ensure!(only_notify, "kraal cannot edit {} safely; {manual}", path.display());
    write_with_backup(path, &text)
}

/// `s` as a TOML basic string, escaping only `\` and `"`: a control character (a newline in a
/// path) goes through as it is, which TOML refuses. Its callers read their edit back as TOML
/// before writing it ([`write_codex_config`], [`write_toml_hooks`]), so such a value stops the
/// edit rather than breaking the file.
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Shells whose prompts and commands kraal can have marked.
pub const SHELLS: &[&str] = &["zsh", "bash", "fish"];

const BLOCK_START: &str = "# >>> kraal shell integration >>>";
const BLOCK_END: &str = "# <<< kraal shell integration <<<";

/// Whether the terminal is kraal, asked by a shell over SSH (where kraal's variables do not reach)
/// through `sh`: kraal answers `OSC 7701 ; ?`, which other terminals ignore, and every terminal
/// answers the request for its attributes (DA1) that follows, which tells when to stop reading.
/// Two seconds without an answer is a no. It reads the terminal, so it runs before the line
/// editor starts, once per shell.
const DETECT: &str = r#"exec </dev/tty >/dev/tty 2>/dev/null || exit 1
s=$(stty -g) || exit 1
trap "stty $s; exit 1" HUP INT TERM
stty -echo -icanon min 0 time 20 || exit 1
e=$(printf "\033")
printf "\033]7701;?\007\033[c"
r=
while c=$(dd bs=256 count=1 2>/dev/null) && [ -n "$c" ]; do
  r=$r$c
  case $r in *"$e[?"*c) break ;; esac
done
stty "$s"
case $r in *"$e]7701;kraal"*) exit 0 ;; esac
exit 1"#;

/// The current context of kubeconfig files and its namespace, read by `awk` as kubectl merges
/// the files: the first `current-context` set wins, and so does the first definition of a
/// context. It reads block YAML as kubectl, kubectx and the clouds' tools write it, comments
/// included, and a context's body written on one line (`context: {namespace: web}`); what it
/// does not follow (JSON, anchors and aliases, a body over several lines in braces) is unknown,
/// and so is a name kraal would not take: a namespace is never taken for absent when it may be
/// there. It goes between single quotes in all three shells: no single quote (`\047` instead)
/// and no double backslash.
const KUBE_AWK: &str = r#"
function unq(v) {
  sub(/^[ \t]+/, "", v); if (v ~ /^#/) v = ""; sub(/[ \t]+#.*$/, "", v); sub(/[ \t]+$/, "", v)
  if (v ~ /^".*"$/ || v ~ /^\047.*\047$/) v = substr(v, 2, length(v) - 2)
  return v
}
function val(l) { sub(/^[^:]*:/, "", l); return unq(l) }
function key(l) { sub(/:([ \t].*)?$/, "", l); return unq(l) }
function flow(v,  n, i, p, f) {
  if (v ~ /^[{][ \t]*[}]$/) return ""
  if (v !~ /^[{].*[}]$/) return "?"
  v = substr(v, 2, length(v) - 2)
  if (v ~ /[][{}&*!]/) return "?"
  n = split(v, p, ","); f = ""
  for (i = 1; i <= n; i++) if (key(p[i]) == "namespace") f = val(p[i])
  return f
}
function flush() {
  if (merge && space == "") space = "?"
  if (name != "" && !(name in ns)) ns[name] = space
  name = ""; space = ""; merge = 0; more = 0
}
function ok(v) { return length(v) <= 200 && v ~ "^[-A-Za-z0-9._:/@+]*$" }
{ sub(/\r$/, "") }
FNR == 1 { flush(); top = ""; item = -1; head = 1 }
head && /[^ \t]/ && !/^[ \t]*#/ { head = 0; if ($0 ~ /^[ \t]*[{[]/) bad = 1 }
/^[^ \t#-]/ {
  flush(); top = key($0); v = val($0)
  if (top == "current-context" && cur == "") cur = v
  if (top == "contexts" && v != "" && v != "[]" && v != "null" && v != "~") bad = 1
  next
}
top == "current-context" && /^[ \t]+[^ \t#]/ { bad = 1 }
top != "contexts" { next }
{
  match($0, /^ */); n = RLENGTH; l = substr($0, n + 1)
  if (l == "" || l ~ /^#/) next
  if (l ~ /^-( |$)/ && (item < 0 || n == item)) {
    flush(); item = n; body = 0; at = -1
    l = substr(l, 2); match(l, /^ */); n += 1 + RLENGTH; l = substr(l, RLENGTH + 1)
    if (l == "" || l ~ /^#/) next
    if (l ~ /^[{*]/) lost = 1
  }
  if (at < 0) at = n
  if (n == at) {
    k = key(l); body = 0; deep = -1; more = 0
    if (k == "name") name = val(l)
    else if (k == "context") { v = val(l); if (v == "") body = 1; else space = flow(v) }
  } else if (body && n > at) {
    if (deep < 0) deep = n
    if (n == deep) {
      k = key(l); more = 0
      if (k == "namespace") { space = val(l); more = (space == "") }
      else if (k == "<<") merge = 1
    } else if (more) space = "?"
  }
}
END {
  flush()
  if (bad) print "k8s=?"
  else if (cur == "") print "k8s="
  else if (!(cur in ns) && lost) print "k8s=?"
  else if (!ok(cur) || !ok(ns[cur])) print "k8s=?"
  else print "k8s=" cur ";ns=" ns[cur]
}
"#;

/// zsh and bash: what the shell works on, reported at each prompt over SSH (`OSC 7701`), as the
/// commands it starts see it: the kubeconfig's current context and namespace, the Terraform
/// workspace of its directory (or `TF_WORKSPACE`), `AWS_PROFILE`. Only the kubeconfig costs a
/// process (`awk`), and only when there is one. A name with other characters than kraal takes
/// is sent as `?`, never as it is.
const SH_CONTEXT: &str = r#"  __kraal_context() {
    local k='k8s=' t= a=${AWS_PROFILE:-${AWS_DEFAULT_PROFILE-}} d=${TF_DATA_DIR:-.terraform} f bad=
    local r=${KUBECONFIG:-$HOME/.kube/config}
    local -a fs=()
    while [[ -n $r ]]; do
      f=${r%%:*}
      [[ $r == *:* ]] && r=${r#*:} || r=
      [[ -n $f && -e $f ]] || continue
      # awk takes `name=value` for an assignment and `-` for its input: never a file's name.
      [[ $f == /* ]] || f=./$f
      if [[ -f $f && -r $f ]]; then fs+=("$f"); else bad=1; fi
    done
    if [[ -n $bad ]]; then
      k='k8s=?'
    elif (( ${#fs[@]} )); then
      k=$(LC_ALL=C command awk '@KUBE_AWK@' "${fs[@]}" </dev/null 2>/dev/null) || k='k8s=?'
    fi
    if [[ -n ${TF_WORKSPACE-} ]]; then
      t=$TF_WORKSPACE
    elif [[ -d $d ]]; then
      if [[ -f $d/environment && -r $d/environment ]]; then
        IFS= read -r t <"$d/environment"
        t=${t:-default}
      elif [[ -e $d/environment ]]; then
        t='?'
      else
        t=default
      fi
    fi
    [[ -z $t || ( ${#t} -le 200 && $t != *[!-A-Za-z0-9._:/@+]* ) ]] || t='?'
    [[ -z $a || ( ${#a} -le 200 && $a != *[!-A-Za-z0-9._:/@+]* ) ]] || a='?'
    printf '\e]7701;context;%s;tf=%s;aws=%s\a' "$k" "$t" "$a"
  }"#;

/// zsh: the mark of a prompt and of a finished command before each prompt (first of the hooks,
/// to see the command's own status), of the command line when the line editor starts, of a
/// command as it runs. In kraal's panes, and over SSH when the terminal says it is kraal.
const ZSH_MARKS: &str = r#"if [[ -z $__kraal_marks ]] && {
  [[ -n $KRAAL_PANE ]] || {
    [[ -o interactive && -n $SSH_CONNECTION && $TERM != (dumb|linux) && -t 0 && -t 1 ]] &&
      command sh -c '@DETECT@' && __kraal_remote=1
  }
}; then
  typeset -g __kraal_marks=1 __kraal_ran=
  __kraal_precmd() {
    local s=$?
    [[ -n $__kraal_ran ]] && printf '\e]133;D;%s\a' $s
    __kraal_ran=
    [[ -n $__kraal_remote ]] && __kraal_context
    printf '\e]133;A\a'
  }
@SH_CONTEXT@
  __kraal_preexec() {
    __kraal_ran=1
    printf '\e]133;C\a'
  }
  __kraal_line_init() {
    printf '\e]133;B\a'
  }
  precmd_functions=(__kraal_precmd $precmd_functions)
  preexec_functions+=(__kraal_preexec)
  autoload -Uz add-zle-hook-widget && add-zle-hook-widget line-init __kraal_line_init
fi
"#;

/// bash: the prompt marks run first in PROMPT_COMMAND, the command line's mark is put back at the
/// end of PS1 after whatever rewrites it, and PS0 (bash 4.4) marks a command as it runs. A
/// PROMPT_COMMAND array (bash 5.1) stays one, with kraal's hooks first and last. In kraal's
/// panes, and over SSH when the terminal says it is kraal.
const BASH_MARKS: &str = r#"if [[ -z $__kraal_marks ]] && {
  [[ -n $KRAAL_PANE ]] || {
    [[ $- == *i* && -n $SSH_CONNECTION && $TERM != dumb && $TERM != linux && -t 0 && -t 1 ]] &&
      command sh -c '@DETECT@' && __kraal_remote=1
  }
}; then
  __kraal_marks=1
  __kraal_prompt() {
    local s=$?
    printf '\e]133;D;%s\a' "$s"
    [[ -n $__kraal_remote ]] && __kraal_context
    printf '\e]133;A\a'
    return $s
  }
@SH_CONTEXT@
  __kraal_input() {
    [[ $PS1 == *'133;B'* ]] || PS1+='\[\e]133;B\a\]'
  }
  if (( BASH_VERSINFO[0] * 100 + BASH_VERSINFO[1] >= 501 )) && [[ ${PROMPT_COMMAND@a} == *a* ]]; then
    PROMPT_COMMAND=(__kraal_prompt "${PROMPT_COMMAND[@]}" __kraal_input)
  else
    PROMPT_COMMAND=$'__kraal_prompt\n'"${PROMPT_COMMAND-}"$'\n__kraal_input'
  fi
  PS0=$'\e]133;C\a'"${PS0-}"
fi
"#;

/// fish: events around each prompt and command (fish has no hook where the command line starts),
/// and over SSH what the shell works on, as for zsh and bash. In kraal's panes, and over SSH
/// when the terminal says it is kraal.
const FISH_MARKS: &str = r#"if not set -q __kraal_marks
    and begin
        set -q KRAAL_PANE
        or begin
            status is-interactive
            and set -q SSH_CONNECTION
            and not contains -- "$TERM" dumb linux
            and test -t 0
            and test -t 1
            and command sh -c '@DETECT@'
            and set -g __kraal_remote 1
        end
    end
    set -g __kraal_marks 1
    function __kraal_prompt --on-event fish_prompt
        if set -q __kraal_status
            printf '\e]133;D;%s\a' $__kraal_status
            set -e __kraal_status
        end
        set -q __kraal_remote; and __kraal_context
        printf '\e]133;A\a'
    end
    function __kraal_preexec --on-event fish_preexec
        printf '\e]133;C\a'
    end
    function __kraal_postexec --on-event fish_postexec
        set -g __kraal_status $status
    end
    function __kraal_context
        set -l k 'k8s='
        set -l t
        set -l a $AWS_PROFILE
        test -n "$a"; or set a $AWS_DEFAULT_PROFILE
        set -l d .terraform
        test -n "$TF_DATA_DIR"; and set d $TF_DATA_DIR
        set -l kc $KUBECONFIG
        test -n "$kc"; or set kc $HOME/.kube/config
        set -l fs
        set -l bad
        for f in (string split : -- $kc)
            test -n "$f"; and test -e "$f"; or continue
            # awk takes `name=value` for an assignment and `-` for its input: never a file's name.
            string match -q -- '/*' $f; or set f ./$f
            if test -f "$f"; and test -r "$f"
                set -a fs $f
            else
                set bad 1
            end
        end
        if set -q bad[1]
            set k 'k8s=?'
        else if set -q fs[1]
            set k (env LC_ALL=C awk '@KUBE_AWK@' $fs </dev/null 2>/dev/null); or set k 'k8s=?'
        end
        if test -n "$TF_WORKSPACE"
            set t $TF_WORKSPACE
        else if test -d "$d"
            if test -f "$d/environment"; and test -r "$d/environment"
                read t <"$d/environment"
                test -n "$t"; or set t default
            else if test -e "$d/environment"
                set t '?'
            else
                set t default
            end
        end
        if test -n "$t"; and not string match -qr -- '^[-A-Za-z0-9._:/@+]{1,200}$' "$t"
            set t '?'
        end
        if test -n "$a"; and not string match -qr -- '^[-A-Za-z0-9._:/@+]{1,200}$' "$a"
            set a '?'
        end
        printf '\e]7701;context;%s;tf=%s;aws=%s\a' "$k" "$t" "$a"
    end
end
"#;

fn shell_rc_path(shell: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    match shell {
        "zsh" => std::env::var_os("ZDOTDIR").map(PathBuf::from).unwrap_or(home).join(".zshrc"),
        "bash" => home.join(".bashrc"),
        _ => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("fish/conf.d/kraal.fish"),
    }
}

fn shell_block(shell: &str) -> String {
    let marks = match shell {
        "zsh" => ZSH_MARKS,
        "bash" => BASH_MARKS,
        _ => FISH_MARKS,
    };
    let marks = marks.replace("@SH_CONTEXT@", SH_CONTEXT).replace("@DETECT@", DETECT).replace("@KUBE_AWK@", KUBE_AWK);
    format!(
        "{BLOCK_START}\n# In kraal's panes, marks prompts and commands (OSC 133); over SSH, also reports the Kubernetes,\n# Terraform and AWS contexts (OSC 7701). Inert elsewhere. `kraal integration uninstall {shell}` removes this.\n{marks}{BLOCK_END}\n"
    )
}

/// `text` without kraal's block, and whether it had one. A block whose end marker is missing (or
/// a second start inside one) is refused: what follows its start belongs to the user.
fn without_block(text: &str, path: &Path) -> Result<(String, bool)> {
    let mut out = Vec::new();
    let mut inside = false;
    let mut found = false;
    for (n, line) in text.lines().enumerate() {
        if line.trim_end() == BLOCK_START {
            anyhow::ensure!(
                !inside,
                "{}: kraal's block is not closed before line {}; remove it by hand, from `{BLOCK_START}` to `{BLOCK_END}`",
                path.display(),
                n + 1
            );
            inside = true;
            found = true;
            // The blank line written before the block goes with it.
            if out.last().is_some_and(|l: &&str| l.trim().is_empty()) {
                out.pop();
            }
        } else if inside && line.trim_end() == BLOCK_END {
            inside = false;
        } else if !inside {
            out.push(line);
        }
    }
    anyhow::ensure!(
        !inside,
        "{}: kraal's block has no `{BLOCK_END}` line; add it at the end of the block, or remove the block by hand",
        path.display()
    );
    let mut rest = out.join("\n");
    if !rest.is_empty() {
        rest.push('\n');
    }
    Ok((rest, found))
}

fn install_shell(shell: &str, path: &Path) -> Result<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (mut rest, _) = without_block(&text, path)?;
    let theirs = !rest.trim().is_empty();
    if !rest.is_empty() {
        rest.push('\n');
    }
    rest.push_str(&shell_block(shell));
    if shell == "fish" && !theirs {
        // A file of kraal's own, in fish's conf.d: nothing of the user's to keep a copy of.
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::paths::write_atomic(path, rest.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
        return Ok(format!(
            "fish: shell integration installed in {}; new shells in kraal mark their commands",
            path.display()
        ));
    }
    write_with_backup(path, &rest)?;
    Ok(format!(
        "{shell}: shell integration installed in {} (backup next to it); new shells in kraal mark their commands",
        path.display()
    ))
}

fn uninstall_shell(shell: &str, path: &Path) -> Result<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(format!("{shell}: nothing to remove")),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (rest, found) = without_block(&text, path)?;
    if !found {
        return Ok(format!("{shell}: nothing to remove"));
    }
    if shell == "fish" && rest.trim().is_empty() {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    } else {
        write_with_backup(path, &rest)?;
    }
    Ok(format!("{shell}: shell integration removed from {}", path.display()))
}

fn shell_status(shell: &str, path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if text.lines().any(|l| l.trim_end() == BLOCK_START) {
        format!("{shell}: installed ({})", path.display())
    } else {
        format!("{shell}: not installed; `kraal integration install {shell}` marks its commands")
    }
}

/// `kraal statusline`: record the usage Claude Code reports for this account, then run the user's
/// own status line (or print a compact one). Never fails: the status line must always render.
pub fn statusline() {
    use std::io::Write;
    let mut input = Vec::new();
    let _ = std::io::stdin().take(1 << 20).read_to_end(&mut input);
    let v: Value = serde_json::from_slice(&input).unwrap_or(Value::Null);
    let all = crate::accounts::Accounts::load().unwrap_or_else(|_| crate::accounts::Accounts::empty());
    let home = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::expand_tilde("~/.claude"));
    let account = all.by_home("claude", &home);
    let usage = crate::accounts::usage_from_statusline(&v);
    if let (Some(a), Some(u)) = (&account, &usage) {
        let _ = crate::accounts::save_usage(&a.agent, &a.name, u);
    }
    match all.claude.statusline.as_deref().filter(|c| !c.trim().is_empty() && !is_our_statusline(c)) {
        Some(cmd) => {
            let child = std::process::Command::new("sh")
                .args(["-c", cmd])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn();
            if let Ok(mut child) = child {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&input);
                }
                let _ = child.wait();
            }
        }
        None => {
            let label = account.as_ref().map(crate::accounts::label).unwrap_or_else(|| "claude".into());
            let text = usage.as_ref().map(|u| crate::server::usage_text(u, crate::accounts::now())).unwrap_or_default();
            if text.is_empty() {
                println!("{label}");
            } else {
                println!("{label} · {text}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kraal-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn claude_install_is_idempotent_and_reversible() {
        let dir = tmp("claude");
        let path = dir.join("settings.json");
        let accounts = dir.join("accounts.toml");
        std::fs::write(&path, r#"{"model":"opus","statusLine":{"type":"command","command":"my-line","padding":1},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}"#).unwrap();
        install_claude(&path, &accounts).unwrap();
        install_claude(&path, &accounts).unwrap();
        let v = read_json(&path).unwrap();
        assert!(is_our_statusline(v["statusLine"]["command"].as_str().unwrap()));
        assert_eq!(v["statusLine"]["padding"], 1);
        let saved = crate::accounts::Accounts::load_from(&accounts).unwrap();
        assert_eq!(saved.claude.statusline.as_deref(), Some("my-line"), "the original survives a second install");
        let v = read_json(&path).unwrap();
        assert_eq!(v["model"], "opus");
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "user hook kept, ours added once: {stop:?}");
        assert!(nested_has(v.get("hooks"), "PreToolUse", "claude"));
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "*");
        // Key order is preserved.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.find("\"model\"").unwrap() < text.find("\"hooks\"").unwrap());
        uninstall_claude(&path, &accounts).unwrap();
        assert_eq!(read_json(&path).unwrap()["statusLine"]["command"], "my-line");
        let v = read_json(&path).unwrap();
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(v["hooks"].get("PreToolUse").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shell_integration_goes_in_once_and_comes_out_whole() {
        let dir = tmp("shells");
        let rc = dir.join(".zshrc");
        std::fs::write(&rc, "export EDITOR=vim\nalias ll='ls -l'").unwrap();
        install_shell("zsh", &rc).unwrap();
        install_shell("zsh", &rc).unwrap();
        let text = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(text.matches(BLOCK_START).count(), 1, "{text}");
        assert!(text.starts_with("export EDITOR=vim\nalias ll='ls -l'\n\n"), "{text}");
        assert!(text.contains("133;D") && text.contains("add-zle-hook-widget"));
        assert!(shell_status("zsh", &rc).contains("installed ("));
        // Kept aside before each change, under a name of its own even for a dotfile.
        assert!(dir.join(".zshrc.kraal-bak").exists());
        std::fs::write(&rc, format!("{text}export LATER=1\n")).unwrap();
        uninstall_shell("zsh", &rc).unwrap();
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "export EDITOR=vim\nalias ll='ls -l'\nexport LATER=1\n");
        assert!(uninstall_shell("zsh", &rc).unwrap().contains("nothing to remove"));
        assert!(shell_status("zsh", &rc).contains("not installed"));
        // fish: a file of its own in conf.d, created with its directory and removed whole.
        let fish = dir.join("config/fish/conf.d/kraal.fish");
        install_shell("fish", &fish).unwrap();
        assert!(std::fs::read_to_string(&fish).unwrap().contains("fish_postexec"));
        uninstall_shell("fish", &fish).unwrap();
        assert!(!fish.exists());
        // One that holds lines of the user's keeps them, and is copied first.
        std::fs::write(&fish, "set -gx EDITOR vim\n").unwrap();
        install_shell("fish", &fish).unwrap();
        assert!(std::fs::read_to_string(&fish).unwrap().starts_with("set -gx EDITOR vim\n\n# >>> kraal"));
        let backup = dir.join("config/fish/conf.d/kraal.fish.kraal-bak");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "set -gx EDITOR vim\n");
        uninstall_shell("fish", &fish).unwrap();
        assert_eq!(std::fs::read_to_string(&fish).unwrap(), "set -gx EDITOR vim\n");
        // bash: what `show` prints is what `install` adds.
        let bashrc = dir.join(".bashrc");
        install_shell("bash", &bashrc).unwrap();
        assert_eq!(std::fs::read_to_string(&bashrc).unwrap().trim_end(), show("bash").unwrap());
        assert!(show("claude").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The shells themselves accept what is installed, and mark a command with its status.
    #[test]
    fn installed_shell_lines_mark_commands() {
        for (shell, args) in [("bash", vec!["--norc", "-i"]), ("zsh", vec!["-f", "-i"])] {
            if std::process::Command::new(shell).arg("-c").arg("true").output().is_err() {
                continue;
            }
            let dir = tmp(&format!("marks-{shell}"));
            let script = dir.join("marks");
            let marks = shell_block(shell);
            std::fs::write(&script, &marks).unwrap();
            let input = format!(". {}\nfalse\nexit\n", script.display());
            let mut child = std::process::Command::new(shell)
                .args(&args)
                .env("KRAAL_PANE", "1")
                .env("PS1", "$ ")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            let all = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
            assert!(all.contains("\x1b]133;A\x07"), "{shell}: {all:?}");
            // Bash marks where output starts from 4.4 on (PS0); macOS ships 3.2.
            let old_bash = shell == "bash"
                && std::process::Command::new("bash")
                    .args(["-c", "(( BASH_VERSINFO[0] * 100 + BASH_VERSINFO[1] < 404 ))"])
                    .status()
                    .is_ok_and(|s| s.success());
            assert!(old_bash || all.contains("\x1b]133;C\x07"), "{shell}: {all:?}");
            assert!(all.contains("\x1b]133;D;1\x07"), "{shell}: {all:?}");
            assert!(!all.contains("syntax error") && !all.contains("parse error"), "{shell}: {all:?}");
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// What the shells' awk program says of kubeconfig files.
    fn kube_report(files: &[&Path]) -> Option<String> {
        let out = std::process::Command::new("awk").arg(KUBE_AWK).args(files).env("LC_ALL", "C").output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// The shells read a kubeconfig as kraal does for local panes, and send `?` for what they
    /// cannot read or kraal would not take.
    #[test]
    fn the_shells_read_kubeconfigs_as_kraal_does() {
        if kube_report(&[]).is_none() {
            return; // no awk here
        }
        let dir = tmp("kubeawk");
        let file = |name: &str, text: &str| {
            let p = dir.join(name);
            std::fs::write(&p, text).unwrap();
            p
        };
        let kubectl = "apiVersion: v1\nclusters:\n- cluster:\n    server: https://1.2.3.4\n  name: prod\ncontexts:\n\
            - context:\n    cluster: prod\n    namespace: payments\n    user: admin\n  name: prod-eks\n\
            - name: \"dev\"\n  context:\n    cluster: dev\n    user: me\n\
            current-context: prod-eks # set by kubectx\nkind: Config\nusers:\n- name: admin\n  user:\n    token: secret\n";
        let minikube = "contexts:\n- context:\n    cluster: minikube\n    extensions:\n    - extension:\n        \
            namespace: trap\n        provider: minikube.sigs.k8s.io\n      name: context_info\n    namespace: default\n    \
            user: minikube\n  name: minikube\ncurrent-context: minikube\n";
        let indented = "# kept by hand\ncontexts:\n  - context:\n      # the team's\n      namespace: web\n    \
            name: 'a'\ncurrent-context: 'a'\n";
        // Comments after the keys, a context's body on one line, Windows line ends.
        let commented = "contexts: # all of them\n- name: shared # the team's\n  context: # team defaults\n    \
            cluster: shared\n    user: operator\n    namespace: prod\ncurrent-context: shared # kubectx\n";
        let inline = "contexts:\n- name: a\n  context: {cluster: c, namespace: \"web\", user: u} # one line\n\
            - context: {cluster: c}\n  name: b\n- context: { }\n  name: c\ncurrent-context: a\n";
        let cases = [
            (vec![file("kubectl", kubectl)], "k8s=prod-eks;ns=payments"),
            (vec![file("minikube", minikube)], "k8s=minikube;ns=default"),
            (vec![file("indented", indented)], "k8s=a;ns=web"),
            (vec![file("commented", commented)], "k8s=shared;ns=prod"),
            (vec![file("inline", inline)], "k8s=a;ns=web"),
            (vec![file("inline-b", &inline.replace("current-context: a", "current-context: b"))], "k8s=b;ns="),
            (vec![file("inline-c", &inline.replace("current-context: a", "current-context: c"))], "k8s=c;ns="),
            (vec![file("crlf", &kubectl.replace('\n', "\r\n"))], "k8s=prod-eks;ns=payments"),
            (
                vec![file("unset", "contexts:\n- name: a\n  context:\n    namespace: x\ncurrent-context: # none\n")],
                "k8s=",
            ),
            (vec![file("dev", &kubectl.replace("current-context: prod-eks", "current-context: dev"))], "k8s=dev;ns="),
            (vec![file("none", "contexts:\n- name: a\n  context: {}\n")], "k8s="),
            // The first file that sets a current context wins, and the first definition.
            (
                vec![
                    file("first", "contexts:\n- name: b\n  context:\n    namespace: nb\ncurrent-context: \"\"\n"),
                    file("second", "contexts:\n- name: b\n  context:\n    namespace: other\ncurrent-context: b\n"),
                ],
                "k8s=b;ns=nb",
            ),
        ];
        for (files, want) in &cases {
            let paths: Vec<&Path> = files.iter().map(|p| p.as_path()).collect();
            assert_eq!(kube_report(&paths).unwrap(), *want, "{files:?}");
            // What kraal reads of the same files, for a local pane.
            let parsed: Vec<_> =
                files.iter().map(|p| crate::context::parse_kubeconfig(&std::fs::read_to_string(p).unwrap())).collect();
            let kraal = crate::context::merge_kubeconfigs(&parsed).resolve(&Default::default());
            let kraal =
                kraal.map_or("k8s=".into(), |k| format!("k8s={};ns={}", k.context, k.namespace.unwrap_or_default()));
            assert_eq!(kraal, *want, "kraal reads {files:?} otherwise");
        }
        // What the shell cannot read, or kraal would not take, is unknown.
        let json = file("json", r#"{"current-context": "prod", "contexts": []}"#);
        let spaced = file("spaced", "current-context: prod eks\n");
        let escape = file("escape", "current-context: a\x1b]133;A\x07b\n");
        let separator = file("separator", "current-context: a;aws=dev\n");
        let long = file("long", &format!("current-context: {}\n", "a".repeat(201)));
        let ns = file("ns", "contexts:\n- name: a\n  context:\n    namespace: \"x y\"\ncurrent-context: a\n");
        // Ways of writing a namespace the shell does not follow: never taken for no namespace.
        let alias =
            file("alias", "x: &c\n  namespace: prod\ncontexts:\n- name: a\n  context: *c\ncurrent-context: a\n");
        let merge = file(
            "merge",
            "x: &c\n  namespace: prod\ncontexts:\n- name: a\n  context:\n    <<: *c\n    user: u\ncurrent-context: a\n",
        );
        let wrapped = file(
            "wrapped",
            "contexts:\n- name: a\n  context: {cluster: c,\n    namespace: prod}\ncurrent-context: a\n",
        );
        let below = file("below", "contexts:\n- name: a\n  context:\n    namespace:\n      prod\ncurrent-context: a\n");
        let list = file("list", "contexts: [{name: a, context: {namespace: prod}}]\ncurrent-context: a\n");
        let item = file("item", "contexts:\n- {name: a, context: {namespace: prod}}\ncurrent-context: a\n");
        let current = file("current", "current-context:\n  prod\n");
        for f in
            [&json, &spaced, &escape, &separator, &long, &ns, &alias, &merge, &wrapped, &below, &list, &item, &current]
        {
            assert_eq!(kube_report(&[f]).unwrap(), "k8s=?", "{f:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What `__kraal_context` of `shell` reports in `dir` (its home too), with `KUBECONFIG` set
    /// to `kubeconfig`.
    fn context_report(shell: &str, dir: &Path, kubeconfig: &str) -> Option<String> {
        let block = dir.join("block");
        std::fs::write(&block, shell_block(shell)).unwrap();
        let (args, source): (&[&str], _) = match shell {
            "bash" => (&["--norc", "--noprofile", "-c"], "."),
            "zsh" => (&["-f", "-c"], "."),
            _ => (&["--no-config", "-c"], "source"),
        };
        let out = std::process::Command::new(shell)
            .args(args)
            .arg(format!("{source} ./block; __kraal_context"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", dir)
            .env("KRAAL_PANE", "1")
            .env("KUBECONFIG", kubeconfig)
            .current_dir(dir)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let start = text.find("\x1b]7701;context;")? + "\x1b]7701;context;".len();
        Some(text[start..].split(';').next()?.to_string() + ";" + text[start..].split(';').nth(1)?)
    }

    /// The names in `KUBECONFIG` are files for awk, whatever they look like: never a variable it
    /// sets (`cur=dev`), nor its input (`-`).
    #[test]
    fn the_shells_give_awk_kubeconfigs_as_files() {
        let dir = tmp("kube-operands");
        let config = "contexts:\n- name: prod\n  context:\n    namespace: web\ncurrent-context: prod\n";
        std::fs::write(dir.join("real.yaml"), config).unwrap();
        std::fs::write(dir.join("cur=dev"), "").unwrap();
        std::fs::write(dir.join("-"), config).unwrap();
        for shell in SHELLS {
            if std::process::Command::new(shell).arg("-c").arg("true").output().is_err() {
                continue;
            }
            for kubeconfig in ["real.yaml:cur=dev", "-", "cur=dev:real.yaml"] {
                assert_eq!(
                    context_report(shell, &dir, kubeconfig).as_deref(),
                    Some("k8s=prod;ns=web"),
                    "{shell} with {kubeconfig}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A shell on a terminal that the test plays (`script` gives it one): it answers the shell's
    /// questions as kraal does, or as another terminal, and keeps what the shell writes.
    #[cfg(target_os = "linux")]
    struct PlayedTerminal {
        child: std::process::Child,
        input: std::process::ChildStdin,
        out: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        answered: usize,
        kraal: bool,
    }

    #[cfg(target_os = "linux")]
    impl PlayedTerminal {
        fn start(command: &str, home: &Path, env: &[(&str, &str)], kraal: bool) -> Option<PlayedTerminal> {
            use std::io::Read;
            let mut cmd = std::process::Command::new("script");
            cmd.args(["-qfc", &format!("stty cols 120 rows 30; exec {command}"), "/dev/null"])
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", home)
                .env("TERM", "xterm-256color")
                .env("SHELL", "/bin/sh")
                .env("LANG", "C.UTF-8")
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env("XDG_CACHE_HOME", home.join(".cache"))
                .env("XDG_RUNTIME_DIR", home)
                .envs(env.iter().copied())
                .current_dir(home)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            let mut child = cmd.spawn().ok()?;
            let input = child.stdin.take()?;
            let mut stdout = child.stdout.take()?;
            let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let kept = out.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    kept.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            });
            Some(PlayedTerminal { child, input, out, answered: 0, kraal })
        }

        fn write(&mut self, bytes: &[u8]) {
            use std::io::Write;
            self.input.write_all(bytes).unwrap();
            self.input.flush().unwrap();
        }

        /// Answer what the shell asked since the last time, in order: kraal's question (as kraal
        /// only) and the attributes (DA1, which every terminal answers).
        fn answer(&mut self) {
            let out = self.out.lock().unwrap().clone();
            let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
            loop {
                let rest = &out[self.answered..];
                let kraal = find(rest, b"\x1b]7701;?\x07").map(|at| (at, 9, true));
                let da1 =
                    [find(rest, b"\x1b[c").map(|at| (at, 3, false)), find(rest, b"\x1b[0c").map(|at| (at, 4, false))];
                let Some((at, len, ours)) = da1.into_iter().chain([kraal]).flatten().min() else { break };
                self.answered += at + len;
                if !ours {
                    self.write(b"\x1b[?62;22c");
                } else if self.kraal {
                    self.write(b"\x1b]7701;kraal\x07");
                }
            }
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
        }

        /// Wait until what the shell wrote since `from` (a length of [`PlayedTerminal::text`])
        /// has `want`.
        fn wait_for(&mut self, from: usize, want: &str) -> String {
            let start = std::time::Instant::now();
            loop {
                self.answer();
                let text = self.text();
                if text.get(from..).is_some_and(|t| t.contains(want)) {
                    return text;
                }
                assert!(
                    start.elapsed() < std::time::Duration::from_secs(10),
                    "no {want:?} in {:?}",
                    &text[from.min(text.len())..]
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        /// Type a command line, and wait for what it prints.
        fn run(&mut self, line: &str, want: &str) -> String {
            let from = self.text().len();
            self.write(format!("{line}\r").as_bytes());
            self.wait_for(from, want)
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for PlayedTerminal {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// `shell` with what `kraal integration show` prints in its startup file, in a home of its
    /// own, on a terminal the test plays, `env` added to its environment. Started: its startup
    /// files are read (what is typed while the shell asks the terminal is lost, as for anyone
    /// typing that early).
    #[cfg(target_os = "linux")]
    fn start_shell(shell: &str, home: &Path, env: &[(&str, &str)], kraal: bool) -> Option<PlayedTerminal> {
        let block = show(shell).unwrap();
        let command = match shell {
            "bash" => {
                std::fs::write(home.join(".bashrc"), format!("PS1='$ '\n{block}\necho STARTED\n")).unwrap();
                format!("bash --rcfile {}/.bashrc -i", home.display())
            }
            "zsh" => {
                std::fs::write(home.join(".zshrc"), format!("PS1='$ '\n{block}\necho STARTED\n")).unwrap();
                "zsh --no-globalrcs -i".to_string()
            }
            _ => {
                let conf = home.join(".config/fish/conf.d");
                std::fs::create_dir_all(&conf).unwrap();
                std::fs::write(conf.join("kraal.fish"), format!("{block}\n")).unwrap();
                std::fs::write(home.join(".config/fish/config.fish"), "set -g fish_greeting\necho STARTED\n").unwrap();
                "fish -i".to_string()
            }
        };
        let mut env = env.to_vec();
        let zdotdir = home.display().to_string();
        env.push(("ZDOTDIR", &zdotdir));
        let mut t = PlayedTerminal::start(&command, home, &env, kraal)?;
        t.wait_for(0, "STARTED");
        Some(t)
    }

    /// Over SSH, the shell asks whether the terminal is kraal; kraal says yes, and the shell marks
    /// its commands and reports at each prompt what it works on, sending `?` for what kraal would
    /// not take. Another terminal is asked and nothing more; outside SSH and kraal, nothing.
    #[cfg(target_os = "linux")]
    fn reports_over_ssh(shell: &str) {
        if std::process::Command::new(shell).arg("-c").arg("true").output().is_err()
            || std::process::Command::new("script").arg("--version").output().is_err()
        {
            return;
        }
        let home = tmp(&format!("remote-{shell}"));
        let kube = "contexts:\n- context:\n    namespace: payments\n  name: prod-eks\n- context: {}\n  name: dev\n\
            current-context: prod-eks\n";
        std::fs::create_dir_all(home.join(".kube")).unwrap();
        std::fs::write(home.join(".kube/config"), kube).unwrap();
        std::fs::write(home.join("dev.yaml"), kube.replace("current-context: prod-eks", "current-context: dev"))
            .unwrap();
        std::fs::write(home.join("evil.yaml"), "current-context: a\x1b]133;A\x07b\n").unwrap();
        std::fs::create_dir_all(home.join("infra/.terraform")).unwrap();
        std::fs::write(home.join("infra/.terraform/environment"), "prod").unwrap();
        let export = |k: &str, v: &str| match shell {
            "fish" => format!("set -gx {k} {v}"),
            _ => format!("export {k}={v}"),
        };
        let ssh = [("SSH_CONNECTION", "10.0.0.1 50000 10.0.0.5 22")];

        // Over SSH, on kraal.
        let mut t = start_shell(shell, &home, &ssh, true).expect("a terminal");
        t.wait_for(0, "\x1b]7701;context;k8s=prod-eks;ns=payments;tf=;aws=\x07\x1b]133;A\x07");
        t.run("echo \"m=$__kraal_marks r=$__kraal_remote\"", "m=1 r=1");
        t.run("cd infra", "\x1b]7701;context;k8s=prod-eks;ns=payments;tf=prod;aws=\x07");
        t.run(&export("AWS_PROFILE", "ops"), ";tf=prod;aws=ops\x07");
        t.run(&export("KUBECONFIG", "$HOME/dev.yaml"), "\x1b]7701;context;k8s=dev;ns=;tf=prod;aws=ops\x07");
        t.run(&export("TF_WORKSPACE", "blue"), ";tf=blue;aws=ops\x07");
        // What kraal would not take is sent as `?`, never as it is.
        t.run(&export("AWS_PROFILE", "'a;b'"), ";tf=blue;aws=?\x07");
        let text = t.run(&export("KUBECONFIG", "$HOME/evil.yaml"), "\x1b]7701;context;k8s=?;tf=blue;aws=?\x07");
        assert!(!text.contains("a\x1b]133;A\x07b"), "{shell}: a name went to the terminal as it is: {text:?}");
        assert!(!text.contains("syntax error") && !text.contains("parse error"), "{shell}: {text:?}");
        drop(t);

        // Over SSH, on another terminal: asked, and nothing more.
        let mut t = start_shell(shell, &home, &ssh, false).expect("a terminal");
        let text = t.run("echo \"m=$__kraal_marks r=$__kraal_remote\"", "m= r=");
        assert!(text.contains("\x1b]7701;?\x07"), "{shell}: {text:?}");
        assert!(!text.contains("\x1b]7701;context"), "{shell}: {text:?}");
        drop(t);

        // Neither over SSH nor in kraal: not even asked.
        let mut t = start_shell(shell, &home, &[], true).expect("a terminal");
        let text = t.run("echo \"m=$__kraal_marks r=$__kraal_remote\"", "m= r=");
        assert!(!text.contains("\x1b]7701"), "{shell}: {text:?}");
        drop(t);

        // In a local kraal pane, even over SSH: marks, without the question or reports (kraal
        // reads the contexts of its own panes itself).
        let mut t = start_shell(shell, &home, &[ssh[0], ("KRAAL_PANE", "1")], true).expect("a terminal");
        t.run("echo \"m=$__kraal_marks r=$__kraal_remote\"", "m=1 r=");
        let text = t.run("cd infra", "\x1b]133;A\x07");
        assert!(!text.contains("\x1b]7701"), "{shell}: {text:?}");
        drop(t);
        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bash_reports_its_contexts_over_ssh_to_kraal_only() {
        reports_over_ssh("bash");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn zsh_reports_its_contexts_over_ssh_to_kraal_only() {
        reports_over_ssh("zsh");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fish_reports_its_contexts_over_ssh_to_kraal_only() {
        reports_over_ssh("fish");
    }

    #[test]
    fn a_shell_file_whose_block_is_cut_is_left_alone() {
        let dir = tmp("cut");
        let rc = dir.join(".zshrc");
        let cut = format!("export A=1\n{BLOCK_START}\nold kraal lines\nexport MINE=1\nalias x=y\n");
        std::fs::write(&rc, &cut).unwrap();
        let err = install_shell("zsh", &rc).unwrap_err().to_string();
        assert!(err.contains(BLOCK_END), "{err}");
        let err = uninstall_shell("zsh", &rc).unwrap_err().to_string();
        assert!(err.contains(BLOCK_END), "{err}");
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), cut);
        // A second start inside the block is refused too.
        let twice = format!("{BLOCK_START}\na\n{BLOCK_START}\nb\n{BLOCK_END}\nexport MINE=1\n");
        std::fs::write(&rc, &twice).unwrap();
        assert!(uninstall_shell("zsh", &rc).is_err());
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), twice);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_linked_shell_file_stays_a_link() {
        let dir = tmp("linked");
        let real = dir.join("dotfiles/zshrc");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "export EDITOR=vim\n").unwrap();
        let rc = dir.join(".zshrc");
        std::os::unix::fs::symlink(&real, &rc).unwrap();
        install_shell("zsh", &rc).unwrap();
        assert!(std::fs::symlink_metadata(&rc).unwrap().file_type().is_symlink());
        assert!(std::fs::read_to_string(&real).unwrap().contains(BLOCK_START));
        assert_eq!(std::fs::read_to_string(dir.join(".zshrc.kraal-bak")).unwrap(), "export EDITOR=vim\n");
        uninstall_shell("zsh", &rc).unwrap();
        assert!(std::fs::symlink_metadata(&rc).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "export EDITOR=vim\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// bash 5.1 runs every element of a PROMPT_COMMAND array: kraal's command-line mark comes
    /// after all of them, even one that sets PS1.
    #[test]
    fn bash_keeps_a_prompt_command_array() {
        let version = std::process::Command::new("bash")
            .arg("-c")
            .arg("echo $(( BASH_VERSINFO[0] * 100 + BASH_VERSINFO[1] ))")
            .output();
        let Ok(version) = version else { return };
        if String::from_utf8_lossy(&version.stdout).trim().parse::<u32>().unwrap_or(0) < 501 {
            return;
        }
        let dir = tmp("bash-array");
        let script = dir.join("marks");
        std::fs::write(&script, shell_block("bash")).unwrap();
        for setup in ["PROMPT_COMMAND=(true 'PS1=\"$ \"')", "PROMPT_COMMAND='PS1=\"$ \"'"] {
            let input = format!("{setup}\n. {}\ntrue\nexit\n", script.display());
            let mut child = std::process::Command::new("bash")
                .args(["--norc", "-i"])
                .env("KRAAL_PANE", "1")
                .env("PS1", "$ ")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            let all = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
            assert!(all.contains("\x1b]133;B\x07"), "{setup}: {all:?}");
            assert!(all.contains("\x1b]133;A\x07"), "{setup}: {all:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_install_goes_before_tables() {
        let dir = tmp("codex");
        let path = dir.join("config.toml");
        std::fs::write(&path, "model = \"o3\"\n\n[mcp_servers.x]\ncommand = \"y\"\n").unwrap();
        install_codex(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert!(parsed.get("notify").is_some(), "{text}");
        assert!(parsed.get("mcp_servers").is_some());
        install_codex(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().matches("notify").count(), 1);
        uninstall_codex(&path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("notify"));
        std::fs::write(&path, "notify = [\"other\"]\n").unwrap();
        assert!(install_codex(&path).is_err());
        // An array written over several lines is replaced, then removed, whole.
        std::fs::write(
            &path,
            "model = \"o3\"\nnotify = [\n  \"/opt/kraal\",\n  \"hook\", \"codex\", # ours\n]\n\n[x]\ny = 1\n",
        )
        .unwrap();
        install_codex(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["notify"].as_array().unwrap().len(), 3, "{text}");
        assert_eq!(parsed["x"]["y"].as_integer(), Some(1));
        std::fs::write(&path, "notify = [\n  \"/opt/kraal\",\n  \"hook\",\n]\nmodel = \"o3\"\n").unwrap();
        uninstall_codex(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed.get("notify").is_none() && parsed.get("model").is_some());
        // A string over several lines that line editing could misread: nothing else is ever lost;
        // the file stays as it was and the user is told what to change.
        let tricky = "notify = [\"/opt/kraal\", \"hook\", \"codex\", \"\"\"\n[\n\"\"\"]\nmodel = \"o3\"\n";
        std::fs::write(&path, tricky).unwrap();
        let err = uninstall_codex(&path).unwrap_err().to_string();
        assert!(err.contains("remove the `notify"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), tricky);
        let err = install_codex(&path).unwrap_err().to_string();
        assert!(err.contains("add `notify = ["), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), tricky);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Gemini CLI, Qwen Code, droid, CodeBuddy and Augment: Claude Code's layout, each with its
    /// own events and timeout unit; the user's settings and hooks stay.
    #[test]
    fn nested_hooks_go_in_once_and_come_out_alone() {
        let dir = tmp("nested");
        for spec in NESTED.iter().filter(|n| n.agent != "claude") {
            let path = dir.join(format!("{}.json", spec.agent));
            let mine = r#"{"model":"x","hooks":{"AfterAgent":[{"hooks":[{"type":"command","command":"notify-send done"}]}],"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#;
            std::fs::write(&path, mine).unwrap();
            let cmd = match spec.agent {
                "auggie" => format!("/home/u/.augment/{AUGGIE_SCRIPT}"),
                agent => hook_command(agent).unwrap(),
            };
            install_nested(spec, &path, false, &cmd).unwrap();
            install_nested(spec, &path, false, &cmd).unwrap();
            let v = read_json(&path).unwrap();
            assert_eq!(v["model"], "x");
            for (event, matcher) in spec.events {
                let groups = v["hooks"][event].as_array().unwrap();
                let ours: Vec<&Value> = groups.iter().filter(|g| runs_ours(&g["hooks"][0], spec.agent)).collect();
                assert_eq!(ours.len(), 1, "{} {event}: {groups:?}", spec.agent);
                assert_eq!(ours[0]["hooks"][0]["timeout"], spec.timeout);
                assert_eq!(ours[0].get("matcher").and_then(Value::as_str), *matcher);
            }
            assert!(status_line(spec.agent, &path, false).unwrap().contains(": installed ("));
            uninstall_nested(spec, &path, false).unwrap();
            assert_eq!(read_json(&path).unwrap(), serde_json::from_str::<Value>(mine).unwrap(), "{}", spec.agent);
            assert!(uninstall_nested(spec, &path, false).unwrap().contains("nothing to remove"));
        }
        // Gemini's timeouts are milliseconds, and its own events.
        let g = nested("gemini");
        assert_eq!(g.timeout, 5000);
        assert!(g.events.iter().any(|(e, _)| *e == "AfterAgent") && !g.events.iter().any(|(e, _)| *e == "Stop"));
        // droid's own hooks.json holds the events at its top level.
        let hooks = dir.join("factory/hooks.json");
        install_nested(nested("droid"), &hooks, true, &hook_command("droid").unwrap()).unwrap();
        let v = read_json(&hooks).unwrap();
        assert!(v.get("hooks").is_none() && nested_has(Some(&v), "Notification", "droid"), "{v}");
        uninstall_nested(nested("droid"), &hooks, true).unwrap();
        assert_eq!(read_json(&hooks).unwrap(), json!({}));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Augment runs a script, not a command line: kraal's own, which its hooks name; an older
    /// kraal's command line is replaced, and the user's hooks stay.
    #[test]
    fn augment_hooks_run_a_script_of_kraals() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("auggie");
        let settings = dir.join("settings.json");
        let script = dir.join(AUGGIE_SCRIPT);
        let mine = r#"{"model":"x","hooks":{"Stop":[{"hooks":[{"type":"command","command":"/home/u/notify.sh","timeout":1000}]}]}}"#;
        let old = r#"{"model":"x","hooks":{"Stop":[{"hooks":[{"type":"command","command":"/home/u/notify.sh","timeout":1000}]},{"hooks":[{"type":"command","command":"'/opt/kraal' hook auggie","timeout":5000}]}]}}"#;
        std::fs::write(&settings, old).unwrap();
        assert!(status_line("auggie", &settings, false).unwrap().contains("not installed"), "Augment cannot run it");
        let content = auggie_script("/opt/my tools/kraal");
        install_auggie(&settings, &script, &content).unwrap();
        install_auggie(&settings, &script, &content).unwrap();
        assert!(content.starts_with("#!/bin/sh\n") && content.ends_with("exec '/opt/my tools/kraal' hook auggie\n"));
        assert_eq!(std::fs::read_to_string(&script).unwrap(), content);
        assert_eq!(std::fs::metadata(&script).unwrap().permissions().mode() & 0o777, 0o755);
        let v = read_json(&settings).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "{stop:?}");
        assert_eq!(stop[1]["hooks"][0]["command"], script.display().to_string());
        assert!(status_line("auggie", &settings, false).unwrap().contains(": installed ("));
        // The script runs, and runs kraal with what it was given.
        let out = std::process::Command::new("sh").arg(&script).output();
        assert!(out.is_ok_and(|o| String::from_utf8_lossy(&o.stderr).contains("/opt/my tools/kraal")));
        uninstall_auggie(&settings, &script, &content).unwrap();
        assert!(!script.exists());
        assert_eq!(read_json(&settings).unwrap(), serde_json::from_str::<Value>(mine).unwrap());
        // A script of the same name that is not kraal's is left alone, and so are the settings.
        std::fs::write(&script, "#!/bin/sh\necho mine\n").unwrap();
        assert!(install_auggie(&settings, &script, &content).is_err());
        assert_eq!(std::fs::read_to_string(&script).unwrap(), "#!/bin/sh\necho mine\n");
        assert_eq!(read_json(&settings).unwrap(), serde_json::from_str::<Value>(mine).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cursor_hooks_keep_the_users() {
        let dir = tmp("cursor");
        let path = dir.join("hooks.json");
        let mine = r#"{"version":1,"hooks":{"afterFileEdit":[{"command":"./hooks/format.sh"}],"stop":[{"command":"./hooks/audit.sh"}]}}"#;
        std::fs::write(&path, mine).unwrap();
        install_cursor(&path).unwrap();
        install_cursor(&path).unwrap();
        let v = read_json(&path).unwrap();
        assert_eq!(v["hooks"]["stop"].as_array().unwrap().len(), 2, "{v}");
        assert!(CURSOR_EVENTS.iter().all(|e| flat_has(v.get("hooks"), e, "cursor")));
        // None of the hooks whose answer Cursor waits for.
        assert!(v["hooks"].get("preToolUse").is_none() && v["hooks"].get("beforeShellExecution").is_none());
        uninstall_cursor(&path).unwrap();
        assert_eq!(read_json(&path).unwrap(), serde_json::from_str::<Value>(mine).unwrap());
        // A file that was not there gets the version Cursor wants.
        let fresh = dir.join("new/hooks.json");
        install_cursor(&fresh).unwrap();
        assert_eq!(read_json(&fresh).unwrap()["version"], 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Copilot's hooks, OpenCode's and Kilo's plugins, pi's extension: files of kraal's own.
    #[test]
    fn own_files_are_written_and_removed_whole() {
        let dir = tmp("own");
        let exe = "/opt/my tools/kraal";
        let copilot = dir.join("copilot/hooks/kraal.json");
        install_own("copilot", &copilot, &copilot_hooks(exe), "hooks").unwrap();
        let v = read_json(&copilot).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["hooks"]["agentStop"][0]["bash"], "'/opt/my tools/kraal' hook copilot agentStop");
        assert_eq!(v["hooks"]["notification"][0]["matcher"], "permission_prompt|elicitation_dialog");
        assert!(v["hooks"].get("preToolUse").is_none() && v["hooks"].get("permissionRequest").is_none());
        for (agent, path, content) in [
            ("opencode", dir.join("opencode/plugins/kraal.js"), opencode_plugin("opencode", exe)),
            ("kilo", dir.join("kilo/plugin/kraal.js"), opencode_plugin("kilo", exe)),
            ("pi", dir.join("pi/extensions/kraal.ts"), pi_extension(exe)),
        ] {
            assert!(content.contains(r#"const kraal = "/opt/my tools/kraal";"#), "{content}");
            install_own(agent, &path, &content, "plugin").unwrap();
            install_own(agent, &path, &content, "plugin").unwrap();
            assert!(std::fs::read_to_string(&path).unwrap().contains(&format!("kraal integration uninstall {agent}")));
            uninstall_own(agent, &path, &content).unwrap();
            assert!(!path.exists());
            assert!(uninstall_own(agent, &path, &content).unwrap().contains("nothing to remove"));
        }
        assert!(opencode_plugin("opencode", exe).contains("export const KraalPlugin"));
        assert!(opencode_plugin("kilo", exe).contains("export default { id: \"kraal\", server: KraalPlugin }"));
        // A file of the same name that is not kraal's is left alone.
        let theirs = dir.join("opencode/plugins/kraal.js");
        let plugin = opencode_plugin("opencode", exe);
        std::fs::write(&theirs, "export const Mine = async () => ({})\n").unwrap();
        assert!(install_own("opencode", &theirs, &plugin, "plugin").is_err());
        assert!(uninstall_own("opencode", &theirs, &plugin).is_err());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "export const Mine = async () => ({})\n");
        uninstall_own("copilot", &copilot, &copilot_hooks(exe)).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A file kraal cannot read may be anyone's: it is never replaced nor removed. One of kraal's
    /// own that was edited is kept aside before it changes or goes.
    #[test]
    fn own_files_kraal_cannot_read_or_that_were_edited_are_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("own-kept");
        let path = dir.join("opencode/plugins/kraal.js");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let plugin = opencode_plugin("opencode", "/opt/kraal");
        let binary = b"\xff\xfe kraal integration uninstall opencode\n".to_vec();
        std::fs::write(&path, &binary).unwrap();
        assert!(install_own("opencode", &path, &plugin, "plugin").is_err());
        assert!(uninstall_own("opencode", &path, &plugin).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), binary);
        std::fs::write(&path, "mine\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads it anyway: then it is simply not kraal's.
        assert!(install_own("opencode", &path, &plugin, "plugin").is_err());
        assert!(uninstall_own("opencode", &path, &plugin).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine\n");
        let backup = dir.join("opencode/plugins/kraal.js.kraal-bak");
        let edited = plugin.replace("const busy", "// mine\n  const busy");
        std::fs::write(&path, &edited).unwrap();
        install_own("opencode", &path, &plugin, "plugin").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), plugin);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), edited);
        std::fs::remove_file(&backup).unwrap();
        install_own("opencode", &path, &plugin, "plugin").unwrap();
        assert!(!backup.exists(), "nothing changed, nothing to keep");
        std::fs::write(&path, &edited).unwrap();
        uninstall_own("opencode", &path, &plugin).unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), edited);
        // Codex's configuration, unreadable as text, is not taken for an empty one.
        let codex = dir.join("config.toml");
        std::fs::write(&codex, b"model = \"\xff\"\n").unwrap();
        assert!(install_codex(&codex).is_err());
        assert_eq!(std::fs::read(&codex).unwrap(), b"model = \"\xff\"\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The plugins are valid JavaScript (and the extension valid once its types are gone), when
    /// Node is there to say so.
    #[test]
    fn plugins_parse() {
        if std::process::Command::new("node").arg("--version").output().is_err() {
            return;
        }
        let dir = tmp("parse");
        let pi =
            pi_extension("/opt/kraal").replace(": any", "").replace(": string", "").replace(": Promise<unknown>", "");
        for (name, code) in [
            ("opencode.mjs", opencode_plugin("opencode", "/opt/kraal")),
            ("kilo.mjs", opencode_plugin("kilo", "/opt/kraal")),
            ("pi.mjs", pi),
        ] {
            let file = dir.join(name);
            std::fs::write(&file, code).unwrap();
            let out = std::process::Command::new("node").arg("--check").arg(&file).output().unwrap();
            assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn kimi_hooks_are_tables_of_their_own() {
        let dir = tmp("kimi");
        let path = dir.join("config.toml");
        let mine = "default_model = \"kimi-k2\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"notify-send done\"\n\n# my theme\n[theme]\nname = \"dark\"\n";
        std::fs::write(&path, mine).unwrap();
        install_kimi(&path).unwrap();
        install_kimi(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        let hooks = parsed["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), KIMI_EVENTS.len() + 1, "{text}");
        // Only the four fields Kimi Code accepts.
        for h in hooks {
            assert!(
                h.as_table().unwrap().keys().all(|k| ["event", "command", "matcher", "timeout"].contains(&k.as_str()))
            );
        }
        assert_eq!(parsed["theme"]["name"].as_str(), Some("dark"));
        assert_eq!(kimi_events(&path).unwrap(), KIMI_EVENTS);
        uninstall_kimi(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), mine);
        assert!(uninstall_kimi(&path).unwrap().contains("nothing to remove"));
        // An inline `hooks = [...]` cannot take tables after it: the file stays as it was.
        let inline = "hooks = [{ event = \"Stop\", command = \"say done\" }]\n";
        std::fs::write(&path, inline).unwrap();
        let err = install_kimi(&path).unwrap_err().to_string();
        assert!(err.contains("add a [[hooks]] table"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), inline);
        // A file that was not there.
        let fresh = dir.join("fresh/config.toml");
        install_kimi(&fresh).unwrap();
        assert!(std::fs::read_to_string(&fresh).unwrap().starts_with("[[hooks]]\nevent = \"SessionStart\""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_agents_are_refused() {
        let err = install("aider").unwrap_err().to_string();
        assert!(err.contains("gemini") && err.contains("fish"), "{err}");
        assert!(status("vim").is_err());
    }

    #[test]
    fn hook_commands_quote_the_path() {
        let cmd = hook_command("claude").unwrap();
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", cmd.trim_end_matches(" hook claude")))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), exe.to_string_lossy());
    }
}
