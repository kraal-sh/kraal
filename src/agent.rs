//! Coding-agent awareness: which pane runs an agent, and whether it is working, blocked on a
//! question, done, or idle.
//!
//! Two sources feed the state. Hooks installed in the agent (`kraal hook claude`) report exact
//! transitions; for agents without hooks, the server watches the foreground process and the
//! screen and infers the state.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    /// Waiting for a prompt, and the user has already seen the last result.
    Idle,
    Working,
    /// Waiting for the user to answer a question or approve an action.
    Blocked,
    /// Finished a turn the user has not looked at yet.
    Done,
    /// Also a state this kraal does not know (a newer kraal's).
    #[default]
    #[serde(other)]
    Unknown,
}

impl AgentState {
    pub fn parse(s: &str) -> Option<AgentState> {
        Some(match s.to_ascii_lowercase().as_str() {
            "unknown" => AgentState::Unknown,
            "idle" => AgentState::Idle,
            "working" | "busy" => AgentState::Working,
            "blocked" | "waiting" | "input" => AgentState::Blocked,
            "done" | "finished" => AgentState::Done,
            _ => return None,
        })
    }

    pub fn icon(self) -> &'static str {
        match self {
            AgentState::Unknown => "·",
            AgentState::Idle => "○",
            AgentState::Working => "●",
            AgentState::Blocked => "▲",
            AgentState::Done => "✓",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentState::Unknown => "unknown",
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Done => "done",
        }
    }

    /// Whether the user should look at this pane.
    pub fn needs_attention(self) -> bool {
        matches!(self, AgentState::Blocked | AgentState::Done)
    }
}

/// Executable names of the agents kraal recognises (or the name of the script an interpreter
/// runs for them), and the name kraal gives each agent.
const KNOWN_AGENTS: &[(&str, &str)] = &[
    ("claude", "claude"),
    ("codex", "codex"),
    ("opencode", "opencode"),
    ("opencode2", "opencode"),
    ("gemini", "gemini"),
    ("aider", "aider"),
    ("cursor-agent", "cursor"),
    ("amp", "amp"),
    ("pi", "pi"),
    ("grok", "grok"),
    ("copilot", "copilot"),
    ("goose", "goose"),
    ("crush", "crush"),
    ("qwen", "qwen"),
    ("kimi", "kimi"),
    ("droid", "droid"),
    ("auggie", "auggie"),
    ("kiro-cli", "kiro"),
    ("kilo", "kilo"),
    ("kilocode", "kilo"),
    ("cline", "cline"),
    ("codebuff", "codebuff"),
    ("freebuff", "codebuff"),
    ("qoder", "qoder"),
    ("qodercli", "qoder"),
    ("codebuddy", "codebuddy"),
    ("junie", "junie"),
    ("llxprt", "llxprt"),
    ("kode", "kode"),
    ("openhands", "openhands"),
    ("vibe-rs", "vibe"),
    // Mistral Vibe's Python launcher renames its process.
    ("Vibe CLI", "vibe"),
];

/// Names other programs use too: they count only as the script of the runtime an agent's
/// package runs on (Letta's server is a Python `letta`, Letta Code a Node one).
const SCRIPT_ONLY: &[(&str, &str, &str)] =
    &[("cn", "node", "continue"), ("letta", "node", "letta"), ("vibe", "python", "vibe")];

/// Agents that run as a subcommand of a broader program: the program, the words that must
/// follow it, and the agent.
const SUBCOMMANDS: &[(&str, &[&str], &str)] = &[("acli", &["rovodev", "run"], "rovodev"), ("q", &["chat"], "kiro")];

const INTERPRETERS: &[&str] = &["node", "bun", "deno", "python", "python3", "uv", "npx", "sh", "bash", "zsh", "dash"];

/// Agent name for a process command line, looking through script interpreters.
pub fn detect_agent(argv: &[String]) -> Option<&'static str> {
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    let first = base(argv.first()?);
    if runs_cursor(argv) {
        return Some("cursor");
    }
    if let Some((_, words, name)) = SUBCOMMANDS.iter().find(|(program, ..)| *program == first) {
        let rest: Vec<&str> = argv.iter().skip(1).filter(|a| !a.starts_with('-')).map(String::as_str).collect();
        return rest.starts_with(words).then_some(*name);
    }
    let interpreted = INTERPRETERS.iter().any(|i| first.starts_with(i));
    let candidates: Vec<String> = if interpreted {
        argv.iter().skip(1).filter(|a| !a.starts_with('-')).take(2).map(|a| base(a)).collect()
    } else {
        vec![first.clone()]
    };
    let script_name =
        |c: &str| [".js", ".mjs", ".cjs"].iter().find_map(|ext| c.strip_suffix(ext)).unwrap_or(c).to_string();
    if interpreted
        && let Some(script) = candidates.first().map(|c| script_name(c))
        && let Some((.., name)) = SCRIPT_ONLY.iter().find(|(s, runtime, _)| *s == script && first.starts_with(runtime))
    {
        return Some(name);
    }
    for c in candidates {
        let c = script_name(&c);
        if let Some((_, name)) = KNOWN_AGENTS.iter().find(|(exe, _)| *exe == c) {
            return Some(name);
        }
    }
    None
}

/// Cursor's launcher runs its bundled Node on the agent's script, under the name it was called
/// by (`agent`, too common a word to go by): Node, by that name or its own, with Cursor's script
/// for the script it runs. A pager or an editor showing that script is not Cursor.
fn runs_cursor(argv: &[String]) -> bool {
    const CURSOR: &str = "/cursor-agent/versions/";
    let Some(program) = argv.first() else { return false };
    let name = program.rsplit('/').next().unwrap_or(program);
    if !(matches!(name, "agent" | "cursor-agent" | "node") || program.contains(CURSOR)) {
        return false;
    }
    // Node's options before the script, with the value those that take one have.
    let mut args = argv.iter().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-r" | "--require" | "--import" | "--loader" | "--experimental-loader" | "--env-file" => {
                args.next();
            }
            "--" => return args.next().is_some_and(|s| s.contains(CURSOR) && s.ends_with("/index.js")),
            "-e" | "--eval" | "-p" | "--print" | "-" => return false,
            _ if a.starts_with('-') => {}
            script => return script.contains(CURSOR) && script.ends_with("/index.js"),
        }
    }
    false
}

/// What an agent shows at the bottom of its screen while it works, and while it waits for
/// the user, beyond what many agents share (`esc to interrupt`, the questions in
/// [`classify_screen`]). From each agent's source, or its documentation when it is closed.
fn screen_patterns(agent: &str) -> (&'static [&'static str], &'static [&'static str]) {
    const GEMINI_ASKS: &[&str] =
        &["apply this change?", "allow execution of", "waiting for user confirmation", "no, suggest changes (esc)"];
    match agent {
        "claude" | "codex" => (&["esc to cancel"], &[]),
        "gemini" | "llxprt" => (&["esc to cancel"], GEMINI_ASKS),
        "qwen" => (
            &["esc to cancel"],
            &["apply this change?", "allow execution of", "yes, allow once", "no, suggest changes (esc)"],
        ),
        "opencode" | "kilo" => (
            &["esc interrupt", "esc again to interrupt", "esc to cancel"],
            &["permission required", "allow once", "allow always", "always allow", "type your own answer"],
        ),
        "crush" => (&[], &["permission required", "allow for session"]),
        "goose" => (&["ctrl+c to interrupt"], &["would like to call the above tool", "do you allow this tool call?"]),
        "pi" => (&["to interrupt)"], &[]),
        "cline" => {
            (&["esc to cancel"], &["cline needs permission", "approve tool call?", "cline is asking a question"])
        }
        "continue" => (&[], &["would you like to continue?"]),
        "openhands" => (&["esc: pause"], &["action(s)?"]),
        "kode" => (&[], &["allow this tool use?", "allow this command?", "allow this edit to"]),
        "vibe" => (
            &["esc/ctrl+c to interrupt"],
            &["permission for the", "allow for remainder of this session", "waiting for approval"],
        ),
        "letta" => (&[], &["run this command?", "don't ask again for this project"]),
        "kimi" => (&["working…"], &["approve once", "reject with feedback"]),
        "aider" => (&[], &["(y)es/(n)o"]),
        "copilot" => (
            &["esc to cancel"],
            &["for the rest of the running session", "no, and tell copilot what to do differently"],
        ),
        "cursor" => (&[], &["skip & tell the agent what to do instead"]),
        "grok" => (&["send a message to interrupt"], &["no, reject (type to add feedback)", "reject once"]),
        "kiro" => (&[], &["allow this action?"]),
        _ => (&[], &[]),
    }
}

/// Infer the state of an agent from the text currently on its screen.
pub fn classify_screen(agent: &str, screen: &str) -> Option<AgentState> {
    // Agents draw their status and prompts at the bottom; older output higher up may quote
    // the same words.
    let tail: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = tail[tail.len().saturating_sub(12)..].join("\n");
    let lower = tail.to_lowercase();
    let (working, asks) = screen_patterns(agent);
    let blocked_common = [
        "do you want to proceed",
        "do you want to make this edit",
        "do you want to create",
        "do you want to allow",
        "would you like to run the following command",
        "would you like to make the following edits",
        "allow command?",
        "approve this",
        "enter to confirm",
        "do you trust",
        "(y/n)",
        "[y/n]",
    ];
    if blocked_common.iter().chain(asks).any(|p| lower.contains(p)) {
        return Some(AgentState::Blocked);
    }
    if ["esc to interrupt"].iter().chain(working).any(|p| lower.contains(p)) {
        return Some(AgentState::Working);
    }
    None
}

/// Per-pane agent bookkeeping.
#[derive(Debug, Clone)]
pub struct AgentTracker {
    pub kind: Option<String>,
    pub state: AgentState,
    pub since: Instant,
    /// Hooks have reported for the current agent run; heuristics are then ignored.
    pub hooked: bool,
    pub session_id: Option<String>,
    pub message: Option<String>,
    /// The process that ran the agent's last hook (its id and start time): the agent itself when
    /// it runs its hooks directly, as OpenCode's and Kilo's plugins do.
    pub reporter: Option<(u32, u64)>,
}

impl Default for AgentTracker {
    fn default() -> Self {
        Self {
            kind: None,
            state: AgentState::Unknown,
            since: Instant::now(),
            hooked: false,
            session_id: None,
            message: None,
            reporter: None,
        }
    }
}

impl AgentTracker {
    /// Set a new state; returns true if it changed.
    pub fn set(&mut self, state: AgentState) -> bool {
        if self.state == state {
            return false;
        }
        self.state = state;
        self.since = Instant::now();
        if state != AgentState::Blocked {
            self.message = None;
        }
        true
    }

    /// The foreground process changed (`agent` is what runs there, if kraal recognises it).
    /// `running` tells whether a process (id, start time) still runs. Returns true if the agent
    /// appeared or went away.
    pub fn set_process(&mut self, agent: Option<&str>, running: impl FnOnce(u32, u64) -> bool) -> bool {
        match (self.kind.as_deref(), agent) {
            (Some(a), Some(b)) if a == b => false,
            (None, None) => false,
            (_, Some(b)) => {
                self.kind = Some(b.to_string());
                self.hooked = false;
                self.reporter = None;
                self.set(AgentState::Idle);
                true
            }
            (Some(kind), None) => {
                // Hooked agents can run under a wrapper kraal does not recognise: keep them while
                // their hooks say when the session ends, and OpenCode and Kilo, whose plugins
                // cannot, while the process their plugin reported from runs.
                let stays = match kind {
                    "opencode" | "kilo" => self.reporter.is_some_and(|(pid, start)| running(pid, start)),
                    _ => true,
                };
                if self.hooked && stays {
                    return false;
                }
                self.kind = None;
                self.reporter = None;
                self.set(AgentState::Unknown);
                true
            }
        }
    }

    /// Heuristic update from the screen and output activity. `quiet` is the time since the
    /// pane last produced output.
    pub fn observe(&mut self, screen: &str, quiet: Duration) -> bool {
        let Some(kind) = self.kind.clone() else { return false };
        if self.hooked {
            return false;
        }
        match classify_screen(&kind, screen) {
            Some(s) => self.set(s),
            None => match self.state {
                AgentState::Working if quiet > Duration::from_millis(1500) => self.set(AgentState::Done),
                AgentState::Blocked if quiet > Duration::from_millis(1500) => self.set(AgentState::Done),
                AgentState::Unknown => self.set(AgentState::Idle),
                _ => false,
            },
        }
    }

    /// The user looked at the pane.
    pub fn mark_seen(&mut self) -> bool {
        if self.state == AgentState::Done {
            return self.set(AgentState::Idle);
        }
        false
    }
}

/// Whether an agent's hooks report every state change (working, blocked, done), so the screen
/// heuristics can be switched off for it. Codex's notify, Cursor's and Augment's hooks have no
/// event for a question to the user, and Gemini CLI's none when one is declined or a tool
/// cancelled: the screen still tells that.
pub fn hooks_cover_lifecycle(agent: &str) -> bool {
    matches!(agent, "claude" | "qwen" | "droid" | "codebuddy" | "copilot" | "kimi" | "opencode" | "kilo" | "pi")
}

/// Map an agent's hook payload to a state change. `event` is the hook's event, for agents that
/// do not name it in the payload (Copilot's own format).
pub fn hook_state(agent: &str, payload: &serde_json::Value, event: Option<&str>) -> Option<AgentState> {
    use AgentState::*;
    let field = |k: &str| payload.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let event = event.unwrap_or_else(|| field("hook_event_name"));
    Some(match (agent, event) {
        // Claude Code's events, and those of the agents that copied them.
        ("claude" | "droid" | "codebuddy" | "qwen", _) => match event {
            "SessionStart" => Idle,
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PreCompact" | "SubagentStart" => Working,
            "Stop" | "StopFailure" => Done,
            "SessionEnd" => Unknown,
            // Waiting for a prompt, which droid and Qwen Code say as soon as a turn ends: what
            // ends a cancelled one, which has no Stop. A turn that finished stays done until
            // seen (see the server's report handling). Claude Code and CodeBuddy say it only
            // after a minute of nothing, which may be a question still waiting.
            "Notification" if field("notification_type") == "idle_prompt" && matches!(agent, "droid" | "qwen") => Idle,
            "Notification" => {
                let msg = field("message").to_lowercase();
                let asks = matches!(field("notification_type"), "permission_prompt" | "elicitation_dialog")
                    || ["permission", "approve", "question"].iter().any(|w| msg.contains(w));
                return asks.then_some(Blocked);
            }
            _ => return None,
        },
        ("gemini", "SessionStart") => Idle,
        ("gemini", "BeforeAgent" | "BeforeTool" | "AfterTool" | "PreCompress") => Working,
        ("gemini", "Notification") if field("notification_type") == "ToolPermission" => Blocked,
        ("gemini", "AfterAgent") => Done,
        ("gemini", "SessionEnd") => Unknown,
        ("kimi", "SessionStart") => Idle,
        ("kimi", "UserPromptSubmit" | "PermissionResult") => Working,
        ("kimi", "PermissionRequest") => Blocked,
        ("kimi", "Stop" | "StopFailure") => Done,
        // Esc: the user is looking at it.
        ("kimi", "Interrupt") => Idle,
        ("kimi", "SessionEnd") => Unknown,
        ("copilot", "sessionStart") => Idle,
        ("copilot", "userPromptSubmitted" | "postToolUse") => Working,
        ("copilot", "notification")
            if matches!(field("notification_type"), "permission_prompt" | "elicitation_dialog") =>
        {
            Blocked
        }
        ("copilot", "agentStop") => Done,
        ("copilot", "sessionEnd") => Unknown,
        ("cursor", "sessionStart") => Idle,
        ("cursor", "beforeSubmitPrompt" | "postToolUse") => Working,
        ("cursor", "stop") if field("status") == "aborted" => Idle,
        ("cursor", "stop") => Done,
        ("cursor", "sessionEnd") => Unknown,
        ("auggie", "SessionStart") => Idle,
        ("auggie", "PreToolUse" | "PostToolUse") => Working,
        ("auggie", "Stop") if field("agent_stop_cause") == "interrupted" => Idle,
        ("auggie", "Stop") => Done,
        ("auggie", "SessionEnd") => Unknown,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn detects_agents_through_interpreters() {
        assert_eq!(detect_agent(&argv("claude")), Some("claude"));
        assert_eq!(detect_agent(&argv("/home/u/.local/bin/claude --resume x")), Some("claude"));
        assert_eq!(detect_agent(&argv("node /usr/lib/node_modules/codex/bin/codex.js")), Some("codex"));
        assert_eq!(detect_agent(&argv("zsh")), None);
        assert_eq!(detect_agent(&argv("ssh claude@host")), None);
    }

    /// Command lines as each agent's package or installer starts it.
    #[test]
    fn detects_agents_as_they_are_installed() {
        let cases = [
            // npm launchers: a Node script named after the agent.
            ("node /usr/bin/gemini --yolo", Some("gemini")),
            ("node /home/u/.npm-global/bin/qwen", Some("qwen")),
            ("node /usr/local/bin/copilot --resume", Some("copilot")),
            ("node /usr/bin/auggie", Some("auggie")),
            ("node /usr/bin/kilocode", Some("kilo")),
            ("node /usr/bin/cline", Some("cline")),
            ("node /usr/bin/codebuff", Some("codebuff")),
            ("node /usr/bin/llxprt", Some("llxprt")),
            ("node /usr/bin/kode", Some("kode")),
            ("node /usr/lib/node_modules/@opencode/cli/bin/opencode.cjs", Some("opencode")),
            ("node /usr/bin/opencode2", Some("opencode")),
            // Native binaries.
            ("/home/u/.local/bin/droid", Some("droid")),
            ("/home/u/.local/bin/kiro-cli", Some("kiro")),
            ("/home/u/.kimi-code/bin/kimi", Some("kimi")),
            ("/home/u/.local/bin/qodercli", Some("qoder")),
            ("/usr/local/bin/codebuddy", Some("codebuddy")),
            ("/home/u/.local/bin/junie", Some("junie")),
            ("/home/u/.local/bin/openhands", Some("openhands")),
            ("/usr/lib/python3.12/site-packages/vibe/_bin/vibe-rs", Some("vibe")),
            // Python entry points.
            ("/home/u/.local/share/uv/tools/aider-chat/bin/python3 /home/u/.local/bin/aider --model x", Some("aider")),
            ("/home/u/.venv/bin/python /home/u/.venv/bin/vibe", Some("vibe")),
            // Cursor's launcher, as `agent` (exec -a) and with its bundled Node.
            (
                "/home/u/.local/bin/agent --use-system-ca /home/u/.local/share/cursor-agent/versions/2026.10.01-e373342/index.js",
                Some("cursor"),
            ),
            (
                "/home/u/.local/share/cursor-agent/versions/2025.09.12-1/node /home/u/.local/share/cursor-agent/versions/2025.09.12-1/index.js",
                Some("cursor"),
            ),
            (
                "node --use-system-ca /home/u/.local/share/cursor-agent/versions/2026.10.01-e373342/index.js",
                Some("cursor"),
            ),
            // Cursor's script in a pager or an editor, or as an argument of another script.
            ("less /home/u/.local/share/cursor-agent/versions/2026.10.01-e373342/index.js", None),
            ("vim -R /home/u/.local/share/cursor-agent/versions/2026.10.01-e373342/index.js", None),
            ("node /srv/app/index.js /home/u/.local/share/cursor-agent/versions/2026.10.01-e373342/index.js", None),
            ("agent -e /home/u/.local/share/cursor-agent/versions/2026.10.01-e373342/index.js", None),
            // Agents behind a broader program's subcommand.
            ("acli rovodev run", Some("rovodev")),
            ("acli jira workitem view X-1", None),
            ("q chat --resume", Some("kiro")),
            ("q", None),
            // Names other programs share count only on the agent's runtime.
            ("node /usr/bin/cn", Some("continue")),
            ("node /usr/bin/letta", Some("letta")),
            ("/usr/bin/python3 /usr/local/bin/letta server", None),
            ("cn", None),
            ("vibe", None),
            ("sh ./cn", None),
            // Neither an editor nor a shell, nor `agent` alone.
            ("nvim src/agent.rs", None),
            ("bash", None),
            ("agent", None),
            ("node /srv/app/index.js", None),
            ("cb", None),
        ];
        for (line, want) in cases {
            assert_eq!(detect_agent(&argv(line)), want, "{line}");
        }
        // The process title Mistral Vibe gives itself: one word with a space in it.
        assert_eq!(detect_agent(&["Vibe CLI".to_string()]), Some("vibe"));
    }

    #[test]
    fn screen_classification() {
        assert_eq!(classify_screen("claude", "✻ Thinking… (esc to interrupt)"), Some(AgentState::Working));
        assert_eq!(classify_screen("claude", "Do you want to proceed?\n❯ 1. Yes"), Some(AgentState::Blocked));
        assert_eq!(classify_screen("claude", "> "), None);
        assert_eq!(
            classify_screen("claude", "❯ No, exit\n  Yes, I trust this folder\nEnter to confirm · Esc to cancel"),
            Some(AgentState::Blocked)
        );
        let old = format!("esc to interrupt\n{}> ", "output\n".repeat(20));
        assert_eq!(classify_screen("claude", &old), None);
    }

    /// Screens built from the text each agent's source (or documentation) draws.
    #[test]
    fn screen_classification_per_agent() {
        use AgentState::{Blocked, Working};
        let cases: &[(&str, &str, Option<AgentState>)] = &[
            ("gemini", "⠏ Reading files (esc to cancel, 12s)", Some(Working)),
            (
                "gemini",
                "Allow execution of [Shell]?\n● 1. Allow once\n  2. Allow for this session\n  4. No, suggest changes (esc)",
                Some(Blocked),
            ),
            ("gemini", "Apply this change?\n● 1. Allow once", Some(Blocked)),
            ("llxprt", "Allow execution of: 'ls'?", Some(Blocked)),
            ("qwen", "⠋ Thinking (3s · ↓ 120 tokens · esc to cancel)", Some(Working)),
            ("qwen", "Allow execution of: 'npm test'?\n● 1. Yes, allow once", Some(Blocked)),
            ("opencode", "┃ Build  claude-sonnet\n esc interrupt", Some(Working)),
            ("opencode", "esc again to interrupt", Some(Working)),
            (
                "opencode",
                "△ Permission required\n→ Allow once   Allow always   Reject\n⇆ select  enter confirm",
                Some(Blocked),
            ),
            ("kilo", "△ Permission required\nAllow   Reject", Some(Blocked)),
            ("crush", "Permission Required\n Allow   Allow for Session   Deny", Some(Blocked)),
            ("goose", "◇ Goose would like to call the above tool, do you allow?\n│ ● Allow", Some(Blocked)),
            ("goose", "Thinking...  (Ctrl+C to interrupt)", Some(Working)),
            ("pi", "⠋ Working (escape to interrupt)", Some(Working)),
            ("cline", "Thinking... (esc to cancel)", Some(Working)),
            ("cline", "Cline needs permission\nApprove tool call?\n[y] Approve  [n] Deny", Some(Blocked)),
            ("continue", "Would you like to continue?\n> Continue (tab)", Some(Blocked)),
            ("continue", "⠋ (4s • esc to interrupt)", Some(Working)),
            ("openhands", "⠋ Working (12s • ESC: pause)", Some(Working)),
            ("openhands", "🔍 Confirm 2 action(s)?\n> Yes\n  No", Some(Blocked)),
            ("kode", "Allow this command?\nEnter to confirm · Esc to reject", Some(Blocked)),
            ("vibe", "Generating… (0s Esc/Ctrl+C to interrupt)", Some(Working)),
            ("vibe", "Permission for the bash tool\n1. Allow once", Some(Blocked)),
            ("letta", "Run this command?\n> Yes\n  Yes, and don't ask again for this project", Some(Blocked)),
            ("kimi", "⠋ Working…", Some(Working)),
            ("kimi", "▶ Run this command?\n> Approve once\n  Approve for this session\n  Reject", Some(Blocked)),
            ("aider", "Run shell command? (Y)es/(N)o/(D)on't ask again [Yes]:", Some(Blocked)),
            ("copilot", "Esc to cancel", Some(Working)),
            (
                "copilot",
                "1. Yes\n2. Yes, and approve shell for the rest of the running session\n3. No, and tell Copilot what to do differently (Esc)",
                Some(Blocked),
            ),
            ("cursor", "  Skip & tell the agent what to do instead", Some(Blocked)),
            ("grok", "Thinking… · send a message to interrupt", Some(Working)),
            (
                "kiro",
                "Allow this action? Use 't' to trust (always allow) this tool for the session. [y/n/t]:",
                Some(Blocked),
            ),
            // An agent's own words do not count for another, and a quiet prompt says nothing.
            ("claude", "esc interrupt", None),
            ("aider", "> ", None),
            ("opencode", "┃ Ask anything", None),
        ];
        for (agent, screen, want) in cases {
            assert_eq!(classify_screen(agent, screen), *want, "{agent}: {screen}");
        }
    }

    #[test]
    fn heuristics_reach_done_after_quiet_working() {
        let mut t = AgentTracker::default();
        t.set_process(Some("claude"), |_, _| false);
        assert_eq!(t.state, AgentState::Idle);
        t.observe("esc to interrupt", Duration::ZERO);
        assert_eq!(t.state, AgentState::Working);
        t.observe("> ", Duration::from_secs(3));
        assert_eq!(t.state, AgentState::Done);
        assert!(t.mark_seen());
        assert_eq!(t.state, AgentState::Idle);
    }

    #[test]
    fn hooks_override_heuristics() {
        let mut t = AgentTracker::default();
        t.set_process(Some("claude"), |_, _| false);
        t.hooked = true;
        t.set(AgentState::Working);
        assert!(!t.observe("> ", Duration::from_secs(10)));
        assert_eq!(t.state, AgentState::Working);
    }

    #[test]
    fn claude_hooks_map() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        let claude = |s: &str| hook_state("claude", &v(s), None);
        assert_eq!(claude(r#"{"hook_event_name":"Stop"}"#), Some(AgentState::Done));
        assert_eq!(
            claude(r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#),
            Some(AgentState::Blocked)
        );
        assert_eq!(claude(r#"{"hook_event_name":"Notification","message":"Claude is waiting for your input"}"#), None);
    }

    /// Payloads as each agent's hook reference describes them.
    #[test]
    fn other_agents_hooks_map() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        let state = |agent: &str, s: &str| hook_state(agent, &v(s), None);
        // Gemini CLI: a tool waiting for approval is a notification of its own type.
        assert_eq!(
            state("gemini", r#"{"hook_event_name":"BeforeAgent","prompt":"fix it"}"#),
            Some(AgentState::Working)
        );
        let permission = r#"{"hook_event_name":"Notification","notification_type":"ToolPermission","message":"Tool Shell requires execution","details":{"type":"exec"}}"#;
        assert_eq!(state("gemini", permission), Some(AgentState::Blocked));
        assert_eq!(
            state("gemini", r#"{"hook_event_name":"AfterAgent","stop_hook_active":false}"#),
            Some(AgentState::Done)
        );
        assert_eq!(state("gemini", r#"{"hook_event_name":"SessionEnd","reason":"exit"}"#), Some(AgentState::Unknown));
        assert_eq!(state("gemini", r#"{"hook_event_name":"Stop"}"#), None);
        // Qwen Code: Claude's names, its own messages; it waits for a prompt as soon as a turn
        // ends, a cancelled one (which has no Stop) included.
        let qwen = r#"{"hook_event_name":"Notification","notification_type":"permission_prompt","title":"Permission needed","message":"Qwen Code needs your permission to use run_shell_command"}"#;
        assert_eq!(state("qwen", qwen), Some(AgentState::Blocked));
        let idle = r#"{"hook_event_name":"Notification","notification_type":"idle_prompt","title":"Waiting for input","message":"Qwen Code is waiting for your input"}"#;
        assert_eq!(state("qwen", idle), Some(AgentState::Idle));
        assert_eq!(state("qwen", r#"{"hook_event_name":"StopFailure"}"#), Some(AgentState::Done));
        // Factory's droid: cancelling a turn is an idle notification, not a stop.
        let droid = r#"{"hook_event_name":"Notification","notification_type":"permission_prompt","message":"Droid is waiting for permission"}"#;
        assert_eq!(state("droid", droid), Some(AgentState::Blocked));
        let cancelled = r#"{"hook_event_name":"Notification","notification_type":"idle_prompt","message":"Droid is waiting for input"}"#;
        assert_eq!(state("droid", cancelled), Some(AgentState::Idle));
        // Claude Code says it after a minute of nothing, a question still waiting maybe.
        let claude_idle = r#"{"hook_event_name":"Notification","notification_type":"idle_prompt","message":"Claude is waiting for your input"}"#;
        assert_eq!(state("claude", claude_idle), None);
        // Kimi Code: approval requests, their answers and Esc have events of their own.
        assert_eq!(
            state("kimi", r#"{"hook_event_name":"PermissionRequest","tool_name":"Shell"}"#),
            Some(AgentState::Blocked)
        );
        assert_eq!(
            state("kimi", r#"{"hook_event_name":"PermissionResult","decision":"approved"}"#),
            Some(AgentState::Working)
        );
        assert_eq!(state("kimi", r#"{"hook_event_name":"Interrupt","reason":"cancelled"}"#), Some(AgentState::Idle));
        // Copilot names the event in its configuration only: kraal passes it along.
        let copilot = |event: &str, s: &str| hook_state("copilot", &v(s), Some(event));
        assert_eq!(copilot("userPromptSubmitted", r#"{"sessionId":"s","prompt":"fix it"}"#), Some(AgentState::Working));
        let asks = r#"{"sessionId":"s","hook_event_name":"Notification","notification_type":"permission_prompt","message":"x"}"#;
        assert_eq!(copilot("notification", asks), Some(AgentState::Blocked));
        let finished = r#"{"sessionId":"s","hook_event_name":"Notification","notification_type":"shell_completed"}"#;
        assert_eq!(copilot("notification", finished), None);
        assert_eq!(copilot("agentStop", r#"{"sessionId":"s","stopReason":"end_turn"}"#), Some(AgentState::Done));
        // Cursor and Augment: turns, no questions.
        assert_eq!(
            state("cursor", r#"{"hook_event_name":"stop","status":"completed","loop_count":0}"#),
            Some(AgentState::Done)
        );
        assert_eq!(
            state("cursor", r#"{"hook_event_name":"beforeSubmitPrompt","prompt":"x"}"#),
            Some(AgentState::Working)
        );
        assert_eq!(state("cursor", r#"{"hook_event_name":"stop","status":"aborted"}"#), Some(AgentState::Idle));
        assert_eq!(
            state("auggie", r#"{"hook_event_name":"Stop","agent_stop_cause":"end_turn"}"#),
            Some(AgentState::Done)
        );
        assert_eq!(
            state("auggie", r#"{"hook_event_name":"Stop","agent_stop_cause":"interrupted"}"#),
            Some(AgentState::Idle)
        );
        assert!(!hooks_cover_lifecycle("cursor") && !hooks_cover_lifecycle("codex") && hooks_cover_lifecycle("qwen"));
        // Gemini CLI has no hook for a tool declined or cancelled: the screen tells that.
        assert!(!hooks_cover_lifecycle("gemini"));
    }

    /// OpenCode under a wrapper kraal does not recognise (`bash -c 'opencode; …'`): it stays
    /// while the process its plugin reported from runs, and goes with it.
    #[test]
    fn a_hooked_opencode_under_a_wrapper_stays_while_it_runs() {
        let hooked = |kind: &str, reporter| AgentTracker {
            kind: Some(kind.into()),
            hooked: true,
            reporter,
            state: AgentState::Working,
            ..Default::default()
        };
        let mut t = hooked("opencode", Some((42, 7)));
        assert!(!t.set_process(None, |pid, start| (pid, start) == (42, 7)));
        assert_eq!((t.kind.as_deref(), t.state), (Some("opencode"), AgentState::Working));
        assert!(t.set_process(None, |_, _| false), "its process ended");
        assert_eq!((t.kind.as_deref(), t.state), (None, AgentState::Unknown));
        // Nothing to go by: it goes, as Kilo does. Hooks that say when the session ends keep
        // the others.
        assert!(hooked("kilo", None).set_process(None, |_, _| true));
        assert!(!hooked("claude", None).set_process(None, |_, _| false));
    }
}
