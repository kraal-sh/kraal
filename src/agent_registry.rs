//! Shared agent detection, launch commands and validated capabilities.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use crate::accounts::AgentKind;

pub struct Agent {
    pub name: &'static str,
    pub label: &'static str,
    pub executables: &'static [&'static str],
    pub scripts: &'static [(&'static str, &'static str)],
    pub subcommands: &'static [&'static [&'static str]],
    pub launches: &'static [&'static [&'static str]],
    /// Presence means native sign-in, reliable verification and documented home isolation.
    pub accounts: Option<AgentKind>,
    pub integration: bool,
    pub reliable_turn_end: bool,
    pub conversation_id: bool,
    pub cross_account_resume: bool,
}

impl Agent {
    pub fn installed_command(&self, cwd: &Path) -> Option<&'static [&'static str]> {
        let path = std::env::var_os("PATH")?;
        self.command_on_path(&path, cwd)
    }

    pub fn command_on_path(&self, path: &std::ffi::OsStr, cwd: &Path) -> Option<&'static [&'static str]> {
        self.launches.iter().copied().find(|argv| {
            let Ok(program) = crate::pane::find_program(argv[0].as_ref(), cwd, Some(path)) else { return false };
            let Some((_, runtime)) = self.scripts.iter().find(|(name, _)| *name == argv[0]) else { return true };
            // Ambiguous names only count on their registered runtime. Inspect the launcher,
            // including symlinks to package entry points, without executing it.
            let Ok(file) = std::fs::File::open(program) else { return false };
            let mut line = String::new();
            if BufReader::new(file.take(4096)).read_line(&mut line).is_err() {
                return false;
            }
            let Some(shebang) = line.strip_prefix("#!") else { return false };
            let mut words = shebang.split_whitespace();
            let Some(interpreter) = words.next() else { return false };
            let interpreter = if Path::new(interpreter).file_name().is_some_and(|n| n == "env") {
                words.find(|word| !word.starts_with('-')).unwrap_or_default()
            } else {
                interpreter
            };
            Path::new(interpreter).file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(runtime))
        })
    }

    pub fn account_message(&self) -> String {
        format!("Accounts are managed in {}", self.label)
    }

    pub fn native_launch(
        &self,
        account: Option<&str>,
        args: &[String],
        cwd: Option<&Path>,
    ) -> anyhow::Result<crate::accounts::Launch> {
        anyhow::ensure!(account.is_none(), "{}", self.account_message());
        let cwd = cwd.map(Path::to_path_buf).unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| "/".into()));
        let command = self.installed_command(&cwd).ok_or_else(|| {
            anyhow::anyhow!(
                "{} is not installed. Install {} and make `{}` available on PATH, then try again.",
                self.label,
                self.label,
                self.launches[0][0]
            )
        })?;
        Ok(crate::accounts::Launch {
            argv: command.iter().map(|s| s.to_string()).chain(args.iter().cloned()).collect(),
            ..Default::default()
        })
    }
}

pub fn find(name: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.name == name)
}

pub const AGENTS: &[Agent] = &[
    Agent {
        name: "claude",
        label: "Claude",
        executables: &["claude"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["claude"]],
        accounts: Some(AgentKind {
            name: "claude",
            command: "claude",
            home_env: "CLAUDE_CONFIG_DIR",
            default_home: "~/.claude",
            login: &["auth", "login"],
            login_status: &["auth", "status"],
            resume: &["--resume", "{session}"],
            session_flag: Some("--session-id"),
            shared: &[
                "CLAUDE.md",
                "settings.json",
                "skills",
                "agents",
                "commands",
                "output-styles",
                "plugins",
                "projects",
                "plans",
            ],
            copied: &[],
            clear_env: &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"],
        }),
        integration: true,
        reliable_turn_end: true,
        conversation_id: true,
        cross_account_resume: true,
    },
    Agent {
        name: "codex",
        label: "Codex",
        executables: &["codex"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["codex"]],
        accounts: Some(AgentKind {
            name: "codex",
            command: "codex",
            home_env: "CODEX_HOME",
            default_home: "~/.codex",
            login: &["login"],
            login_status: &["login", "status"],
            resume: &["resume", "{session}"],
            session_flag: None,
            // Codex keeps databases and locks in its home: nothing is shared.
            shared: &[],
            copied: &["config.toml"],
            clear_env: &["OPENAI_API_KEY", "CODEX_API_KEY"],
        }),
        integration: true,
        reliable_turn_end: false,
        conversation_id: true,
        cross_account_resume: false,
    },
    Agent {
        name: "opencode",
        label: "OpenCode",
        executables: &["opencode", "opencode2"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["opencode"], &["opencode2"]],
        accounts: None,
        integration: true,
        reliable_turn_end: true,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "gemini",
        label: "Gemini",
        executables: &["gemini"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["gemini"]],
        accounts: None,
        integration: true,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "aider",
        label: "Aider",
        executables: &["aider"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["aider"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "cursor",
        label: "Cursor",
        executables: &["cursor-agent"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["cursor-agent"]],
        accounts: None,
        integration: true,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "amp",
        label: "Amp",
        executables: &["amp"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["amp"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "pi",
        label: "pi",
        executables: &["pi"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["pi"]],
        accounts: None,
        integration: true,
        reliable_turn_end: true,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "grok",
        label: "Grok",
        executables: &["grok"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["grok"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "copilot",
        label: "Copilot",
        executables: &["copilot"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["copilot"]],
        accounts: None,
        integration: true,
        reliable_turn_end: true,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "goose",
        label: "Goose",
        executables: &["goose"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["goose"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "crush",
        label: "Crush",
        executables: &["crush"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["crush"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "qwen",
        label: "Qwen",
        executables: &["qwen"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["qwen"]],
        accounts: None,
        integration: true,
        reliable_turn_end: true,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "kimi",
        label: "Kimi",
        executables: &["kimi"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["kimi"]],
        accounts: None,
        integration: true,
        reliable_turn_end: true,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "droid",
        label: "droid",
        executables: &["droid"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["droid"]],
        accounts: None,
        integration: true,
        reliable_turn_end: true,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "auggie",
        label: "Augment",
        executables: &["auggie"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["auggie"]],
        accounts: None,
        integration: true,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "kiro",
        label: "Kiro",
        executables: &["kiro-cli"],
        scripts: &[],
        subcommands: &[&["q", "chat"]],
        launches: &[&["kiro-cli"], &["q", "chat"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "kilo",
        label: "Kilo",
        executables: &["kilo", "kilocode"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["kilo"], &["kilocode"]],
        accounts: None,
        integration: true,
        reliable_turn_end: true,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "cline",
        label: "Cline",
        executables: &["cline"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["cline"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "codebuff",
        label: "Codebuff",
        executables: &["codebuff", "freebuff"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["codebuff"], &["freebuff"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "qoder",
        label: "Qoder",
        executables: &["qoder", "qodercli"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["qoder"], &["qodercli"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "codebuddy",
        label: "CodeBuddy",
        executables: &["codebuddy"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["codebuddy"]],
        accounts: None,
        integration: true,
        reliable_turn_end: true,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "junie",
        label: "Junie",
        executables: &["junie"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["junie"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "llxprt",
        label: "LLxprt",
        executables: &["llxprt"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["llxprt"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "kode",
        label: "Kode",
        executables: &["kode"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["kode"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "openhands",
        label: "OpenHands",
        executables: &["openhands"],
        scripts: &[],
        subcommands: &[],
        launches: &[&["openhands"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "vibe",
        label: "Vibe",
        executables: &["vibe-rs", "Vibe CLI"],
        scripts: &[("vibe", "python")],
        subcommands: &[],
        launches: &[&["vibe-rs"], &["vibe"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "continue",
        label: "Continue",
        executables: &[],
        scripts: &[("cn", "node")],
        subcommands: &[],
        launches: &[&["cn"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "letta",
        label: "Letta",
        executables: &[],
        scripts: &[("letta", "node")],
        subcommands: &[],
        launches: &[&["letta"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
    Agent {
        name: "rovodev",
        label: "Rovo Dev",
        executables: &[],
        scripts: &[],
        subcommands: &[&["acli", "rovodev", "run"]],
        launches: &[&["acli", "rovodev", "run"]],
        accounts: None,
        integration: false,
        reliable_turn_end: false,
        conversation_id: false,
        cross_account_resume: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_launches_and_detection_agree() {
        let mut names = std::collections::HashSet::new();
        for agent in AGENTS {
            assert!(names.insert(agent.name), "duplicate {}", agent.name);
            assert!(!agent.launches.is_empty(), "{} cannot launch", agent.name);
            for command in agent.launches {
                let mut argv: Vec<String> = command.iter().map(|s| s.to_string()).collect();
                if let Some((_, runtime)) = agent.scripts.iter().find(|(s, _)| *s == command[0]) {
                    argv.insert(0, runtime.to_string());
                }
                assert_eq!(crate::agent::detect_agent(&argv), Some(agent.name), "{command:?}");
            }
            for executable in agent.executables {
                assert_eq!(crate::agent::detect_agent(&[executable.to_string()]), Some(agent.name));
            }
            assert_eq!(crate::agent::hooks_cover_lifecycle(agent.name), agent.reliable_turn_end);
            assert!(!agent.reliable_turn_end || agent.integration);
            if let Some(account) = &agent.accounts {
                assert_eq!(account.name, agent.name);
                assert_eq!(agent.launches[0], [account.command]);
                assert!(!account.login.is_empty() && !account.login_status.is_empty());
                assert!(!account.home_env.is_empty());
                assert!(agent.conversation_id);
            }
        }
        assert_eq!(crate::accounts::agents().map(|a| a.name).collect::<Vec<_>>(), ["claude", "codex"]);
    }

    #[test]
    fn documented_capabilities_match_the_registry() {
        let docs = include_str!("../docs/agents.md");
        let yes = |value| if value { "Yes" } else { "No" };
        for agent in AGENTS {
            let commands = agent.launches.iter().map(|c| format!("`{}`", c.join(" "))).collect::<Vec<_>>().join(", ");
            let accounts = yes(agent.accounts.is_some());
            let row = format!(
                "| `{}` | {commands} | {accounts} | {accounts} | {accounts} | {} | {} | {} |",
                agent.name,
                yes(agent.conversation_id),
                yes(agent.cross_account_resume),
                yes(agent.reliable_turn_end)
            );
            assert!(docs.contains(&row), "missing or stale capabilities: {row}");
        }
    }

    #[test]
    fn executable_aliases_and_subcommands_are_resolved_without_running_them() {
        let dir = std::env::temp_dir().join(format!("kraal-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.as_os_str();
        let open = find("opencode").unwrap();
        assert!(open.command_on_path(path, &dir).is_none());
        std::fs::write(dir.join("opencode"), "not executable").unwrap();
        std::fs::create_dir(dir.join("gemini")).unwrap();
        assert!(open.command_on_path(path, &dir).is_none());
        assert!(find("gemini").unwrap().command_on_path(path, &dir).is_none());
        for name in ["opencode2", "acli", "q", "vibe"] {
            crate::testing::write_script(&dir.join(name), "#!/bin/sh\nexit 99\n");
        }
        assert_eq!(open.command_on_path(path, &dir), Some(&["opencode2"][..]));
        assert_eq!(find("rovodev").unwrap().command_on_path(path, &dir), Some(&["acli", "rovodev", "run"][..]));
        assert_eq!(find("kiro").unwrap().command_on_path(path, &dir), Some(&["q", "chat"][..]));
        crate::testing::write_script(&dir.join("vibe"), "#!/usr/bin/env python3\nexit(99)\n");
        assert_eq!(find("vibe").unwrap().command_on_path(path, &dir), Some(&["vibe"][..]));
        crate::testing::write_script(&dir.join("opencode"), "#!/bin/sh\nexit 99\n");
        assert_eq!(open.command_on_path(path, &dir), Some(&["opencode"][..]));
        assert!(find("aider").unwrap().command_on_path(path, &dir).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_native_agent_refuses_a_managed_account() {
        let agent = find("gemini").unwrap();
        assert!(crate::accounts::agent_kind(agent.name).is_none());
        assert!(!agent.conversation_id && !agent.cross_account_resume);
        assert_eq!(
            agent.native_launch(Some("work"), &[], None).unwrap_err().to_string(),
            "Accounts are managed in Gemini"
        );
        let mut all = crate::accounts::Accounts::empty();
        let mut account = all.get("claude", "default").unwrap();
        account.agent = "gemini".into();
        assert_eq!(all.add(account).unwrap_err().to_string(), "Accounts are managed in Gemini");
        assert!(all.all().is_empty());
        assert_eq!(crate::accounts::missing_account("gemini", "work"), "Accounts are managed in Gemini");
    }
}
