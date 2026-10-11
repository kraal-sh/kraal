//! Agent hooks: `kraal hook <agent>` reports state changes to the server, and
//! `kraal integration install <agent>` wires that command into the agent's configuration.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::agent::{AgentState, claude_hook_state};
use crate::client;
use crate::protocol::Request;

/// Claude Code events kraal listens to.
const CLAUDE_EVENTS: &[(&str, bool)] = &[
    ("SessionStart", false),
    ("UserPromptSubmit", false),
    ("PreToolUse", true),
    ("PostToolUse", true),
    ("Notification", false),
    ("Stop", false),
    ("SessionEnd", false),
];

/// Entry point of `kraal hook <agent>`. Never fails loudly and never prints: agents treat hook
/// output and exit codes as instructions.
pub fn run_hook(agent: &str, arg: Option<String>) {
    let (Some(pane), Some(socket)) = (std::env::var_os("KRAAL_PANE"), std::env::var_os("KRAAL_SOCKET")) else {
        return; // not running inside kraal
    };
    let pane = pane.to_string_lossy().into_owned();
    let socket = PathBuf::from(socket);
    let report = match agent {
        "claude" => {
            let mut input = String::new();
            let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
            let Ok(payload) = serde_json::from_str::<Value>(&input) else { return };
            let state = claude_hook_state(&payload);
            let session_id = payload.get("session_id").and_then(Value::as_str).map(String::from);
            let message = payload.get("message").and_then(Value::as_str).map(String::from);
            if state.is_none() && session_id.is_none() {
                return;
            }
            Request::AgentReport { pane, agent: "claude".into(), state, session_id, message }
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

/// The command the agent's hooks run: a shell parses it, so the path is quoted whatever it holds.
fn hook_command(agent: &str) -> Result<String> {
    let exe = std::env::current_exe()?;
    Ok(format!("{} hook {agent}", crate::shell::quote(&exe.to_string_lossy())))
}

fn is_ours(cmd: &str, agent: &str) -> bool {
    cmd.contains("kraal") && cmd.ends_with(&format!(" hook {agent}"))
}

pub fn install(agent: &str) -> Result<String> {
    match agent {
        "claude" => install_claude(&claude_settings_path(), &crate::accounts::Accounts::path()),
        "codex" => install_codex(&codex_config_path()),
        _ => bail!("unknown agent {agent:?} (supported: claude, codex)"),
    }
}

pub fn uninstall(agent: &str) -> Result<String> {
    match agent {
        "claude" => uninstall_claude(&claude_settings_path(), &crate::accounts::Accounts::path()),
        "codex" => uninstall_codex(&codex_config_path()),
        _ => bail!("unknown agent {agent:?} (supported: claude, codex)"),
    }
}

pub fn status(agent: &str) -> Result<String> {
    match agent {
        "claude" => {
            let path = claude_settings_path();
            let v = read_json(&path)?;
            let installed: Vec<&str> =
                CLAUDE_EVENTS.iter().filter(|(e, _)| claude_has(&v, e)).map(|(e, _)| *e).collect();
            Ok(match installed.len() {
                0 => format!("claude: not installed ({})", path.display()),
                n if n == CLAUDE_EVENTS.len() => format!("claude: installed ({})", path.display()),
                _ => format!(
                    "claude: partially installed ({}); run `kraal integration install claude`",
                    installed.join(", ")
                ),
            })
        }
        "codex" => {
            let path = codex_config_path();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            Ok(if text.lines().any(|l| l.trim_start().starts_with("notify") && l.contains("kraal")) {
                format!("codex: installed ({})", path.display())
            } else {
                format!("codex: not installed ({})", path.display())
            })
        }
        _ => bail!("unknown agent {agent:?}"),
    }
}

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(json!({})),
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_with_backup(path: &Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        let backup =
            path.with_extension(format!("{}.kraal-bak", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
        std::fs::copy(path, &backup).with_context(|| format!("backing up {}", path.display()))?;
    }
    crate::paths::write_atomic(path, content.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

fn claude_has(v: &Value, event: &str) -> bool {
    v.pointer(&format!("/hooks/{event}")).and_then(Value::as_array).is_some_and(|groups| {
        groups.iter().any(|g| {
            g.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
                hs.iter().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| is_ours(c, "claude")))
            })
        })
    })
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
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().context("\"hooks\" is not an object")?;
    let mut added = 0;
    for (event, needs_matcher) in CLAUDE_EVENTS {
        let groups = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        let groups = groups.as_array_mut().with_context(|| format!("hooks.{event} is not an array"))?;
        // Drop stale kraal entries (e.g. an old binary path), then add ours.
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                hs.retain(|h| !h.get("command").and_then(Value::as_str).is_some_and(|c| is_ours(c, "claude")));
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|hs| !hs.is_empty()));
        let mut group = json!({ "hooks": [{ "type": "command", "command": cmd, "timeout": 5 }] });
        if *needs_matcher {
            group["matcher"] = json!("*");
        }
        groups.push(group);
        added += 1;
    }
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
        for (_, groups) in hooks.iter_mut() {
            let Some(groups) = groups.as_array_mut() else { continue };
            for g in groups.iter_mut() {
                if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                    let before = hs.len();
                    hs.retain(|h| !h.get("command").and_then(Value::as_str).is_some_and(|c| is_ours(c, "claude")));
                    removed += before - hs.len();
                }
            }
            groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|hs| !hs.is_empty()));
        }
        hooks.retain(|_, groups| groups.as_array().is_none_or(|a| !a.is_empty()));
    }
    if removed == 0 {
        return Ok("claude: nothing to remove".into());
    }
    write_with_backup(path, &(serde_json::to_string_pretty(&v)? + "\n"))?;
    Ok(format!("claude: removed {removed} hooks from {}", path.display()))
}

fn install_codex(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
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
    let text = std::fs::read_to_string(path).unwrap_or_default();
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

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
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
        assert!(claude_has(&v, "PreToolUse"));
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
