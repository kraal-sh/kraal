//! Agent hooks: `kraal hook <agent>` reports state changes to the server, and
//! `kraal integration install <agent>` wires that command into the agent's configuration. For
//! shells, `kraal integration install <shell>` adds the lines that mark prompts and commands
//! (OSC 133) to the shell's startup file.

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
        s if SHELLS.contains(&s) => install_shell(s, &shell_rc_path(s)),
        _ => bail!("unknown agent or shell {agent:?} (supported: claude, codex, zsh, bash, fish)"),
    }
}

pub fn uninstall(agent: &str) -> Result<String> {
    match agent {
        "claude" => uninstall_claude(&claude_settings_path(), &crate::accounts::Accounts::path()),
        "codex" => uninstall_codex(&codex_config_path()),
        s if SHELLS.contains(&s) => uninstall_shell(s, &shell_rc_path(s)),
        _ => bail!("unknown agent or shell {agent:?} (supported: claude, codex, zsh, bash, fish)"),
    }
}

/// What `install` adds to a shell's startup file, for those who keep it themselves.
pub fn show(shell: &str) -> Result<String> {
    if !SHELLS.contains(&shell) {
        bail!("nothing to show for {shell:?}: zsh, bash or fish");
    }
    Ok(shell_block(shell).trim_end().to_string())
}

pub fn status(agent: &str) -> Result<String> {
    match agent {
        s if SHELLS.contains(&s) => Ok(shell_status(s, &shell_rc_path(s))),
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

/// Shells whose prompts and commands kraal can have marked.
pub const SHELLS: &[&str] = &["zsh", "bash", "fish"];

const BLOCK_START: &str = "# >>> kraal shell integration >>>";
const BLOCK_END: &str = "# <<< kraal shell integration <<<";

/// zsh: the mark of a prompt and of a finished command before each prompt (first of the hooks,
/// to see the command's own status), of the command line when the line editor starts, of a
/// command as it runs.
const ZSH_MARKS: &str = r#"if [[ -n $KRAAL_PANE && -z $__kraal_marks ]]; then
  typeset -g __kraal_marks=1 __kraal_ran=
  __kraal_precmd() {
    local s=$?
    [[ -n $__kraal_ran ]] && printf '\e]133;D;%s\a' $s
    __kraal_ran=
    printf '\e]133;A\a'
  }
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
/// PROMPT_COMMAND array (bash 5.1) stays one, with kraal's hooks first and last.
const BASH_MARKS: &str = r#"if [[ -n $KRAAL_PANE && -z $__kraal_marks ]]; then
  __kraal_marks=1
  __kraal_prompt() {
    local s=$?
    printf '\e]133;D;%s\a\e]133;A\a' "$s"
    return $s
  }
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

/// fish: events around each prompt and command (fish has no hook where the command line starts).
const FISH_MARKS: &str = r#"if set -q KRAAL_PANE; and not set -q __kraal_marks
    set -g __kraal_marks 1
    function __kraal_prompt --on-event fish_prompt
        if set -q __kraal_status
            printf '\e]133;D;%s\a' $__kraal_status
            set -e __kraal_status
        end
        printf '\e]133;A\a'
    end
    function __kraal_preexec --on-event fish_preexec
        printf '\e]133;C\a'
    end
    function __kraal_postexec --on-event fish_postexec
        set -g __kraal_status $status
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
    format!(
        "{BLOCK_START}\n# In kraal's panes, marks prompts and commands (OSC 133): `kraal integration uninstall {shell}` removes this.\n{marks}{BLOCK_END}\n"
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
    if !rest.is_empty() {
        rest.push('\n');
    }
    rest.push_str(&shell_block(shell));
    if shell == "fish" {
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
            assert!(all.contains("\x1b]133;C\x07"), "{shell}: {all:?}");
            assert!(all.contains("\x1b]133;D;1\x07"), "{shell}: {all:?}");
            assert!(!all.contains("syntax error") && !all.contains("parse error"), "{shell}: {all:?}");
            let _ = std::fs::remove_dir_all(dir);
        }
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

    #[cfg(unix)]
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
