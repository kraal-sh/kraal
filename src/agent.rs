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
    #[default]
    Unknown,
    /// Waiting for a prompt, and the user has already seen the last result.
    Idle,
    Working,
    /// Waiting for the user to answer a question or approve an action.
    Blocked,
    /// Finished a turn the user has not looked at yet.
    Done,
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

/// Executable names of the agents kraal recognises.
const KNOWN_AGENTS: &[(&str, &str)] = &[
    ("claude", "claude"),
    ("codex", "codex"),
    ("opencode", "opencode"),
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
];

const INTERPRETERS: &[&str] = &["node", "bun", "deno", "python", "python3", "uv", "npx", "sh", "bash", "zsh", "dash"];

/// Agent name for a process command line, looking through script interpreters.
pub fn detect_agent(argv: &[String]) -> Option<&'static str> {
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    let first = base(argv.first()?);
    let candidates: Vec<String> = if INTERPRETERS.iter().any(|i| first.starts_with(i)) {
        argv.iter().skip(1).filter(|a| !a.starts_with('-')).take(2).map(|a| base(a)).collect()
    } else {
        vec![first]
    };
    for c in candidates {
        let c = c.trim_end_matches(".js").trim_end_matches(".mjs");
        if let Some((_, name)) = KNOWN_AGENTS.iter().find(|(exe, _)| *exe == c) {
            return Some(name);
        }
    }
    None
}

/// Infer the state of an agent from the text currently on its screen.
pub fn classify_screen(agent: &str, screen: &str) -> Option<AgentState> {
    // Agents draw their status and prompts at the bottom; older output higher up may quote
    // the same words.
    let tail: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = tail[tail.len().saturating_sub(12)..].join("\n");
    let lower = tail.to_lowercase();
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
    if blocked_common.iter().any(|p| lower.contains(p)) {
        return Some(AgentState::Blocked);
    }
    let working = match agent {
        "claude" | "codex" | "opencode" | "gemini" | "qwen" => {
            lower.contains("esc to interrupt") || lower.contains("esc to cancel")
        }
        _ => lower.contains("esc to interrupt"),
    };
    if working {
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

    /// The foreground process changed. Returns true if the agent appeared or went away.
    pub fn set_process(&mut self, agent: Option<&str>) -> bool {
        match (self.kind.as_deref(), agent) {
            (Some(a), Some(b)) if a == b => false,
            (None, None) => false,
            (_, Some(b)) => {
                self.kind = Some(b.to_string());
                self.hooked = false;
                self.set(AgentState::Idle);
                true
            }
            (Some(_), None) => {
                // Hooked agents can run under a wrapper we don't recognise; keep them.
                if self.hooked {
                    return false;
                }
                self.kind = None;
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
/// heuristics can be switched off for it.
pub fn hooks_cover_lifecycle(agent: &str) -> bool {
    agent == "claude"
}

/// Map a Claude Code hook payload to a state change.
pub fn claude_hook_state(payload: &serde_json::Value) -> Option<AgentState> {
    let event = payload.get("hook_event_name")?.as_str()?;
    Some(match event {
        "SessionStart" => AgentState::Idle,
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PreCompact" | "SubagentStart" => AgentState::Working,
        "Stop" => AgentState::Done,
        "SessionEnd" => AgentState::Unknown,
        "Notification" => {
            let kind = payload.get("notification_type").and_then(|v| v.as_str()).unwrap_or("");
            let msg = payload.get("message").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            let asks = matches!(kind, "permission_prompt" | "elicitation_dialog")
                || ["permission", "approve", "question"].iter().any(|w| msg.contains(w));
            if asks {
                AgentState::Blocked
            } else {
                return None;
            }
        }
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

    #[test]
    fn heuristics_reach_done_after_quiet_working() {
        let mut t = AgentTracker::default();
        t.set_process(Some("claude"));
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
        t.set_process(Some("claude"));
        t.hooked = true;
        t.set(AgentState::Working);
        assert!(!t.observe("> ", Duration::from_secs(10)));
        assert_eq!(t.state, AgentState::Working);
    }

    #[test]
    fn claude_hooks_map() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(claude_hook_state(&v(r#"{"hook_event_name":"Stop"}"#)), Some(AgentState::Done));
        assert_eq!(
            claude_hook_state(&v(
                r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#
            )),
            Some(AgentState::Blocked)
        );
        assert_eq!(
            claude_hook_state(&v(r#"{"hook_event_name":"Notification","message":"Claude is waiting for your input"}"#)),
            None
        );
    }
}
