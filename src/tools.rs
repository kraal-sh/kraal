//! Tools: programs one key away (lazygit, k9s, htop, logs…), started where the focused pane
//! works, on its server or in its directory, with its context in the environment.
//!
//! Tools are declared in the `[tools]` section of the config. kraal ships a few that are
//! hidden when their program is not installed; none is required.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::context::KubeContext;
use crate::git::GitInfo;
use crate::pane::PaneSpec;
use crate::protocol::Placement;
use crate::servers::Server;
use crate::shell;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDef {
    /// A shell command line, run by `sh -c`.
    pub command: String,
    #[serde(default)]
    pub description: String,
    /// A key after the prefix that starts the tool directly.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub on: Where,
    #[serde(default)]
    pub open: Open,
    /// Keep the pane open after the command ends, to read its output.
    #[serde(default)]
    pub hold: bool,
    /// What the focused pane must have for the tool to be offered.
    #[serde(default)]
    pub when: Vec<Need>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// The tool only looks (htop, logs): it starts on a production pane without asking.
    #[serde(default)]
    pub read_only: bool,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Where {
    /// Where the focused pane is: on its server over SSH, or locally in its directory.
    #[default]
    Pane,
    /// Always on this machine (in the pane's directory when it is local).
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Open {
    #[default]
    Floating,
    Right,
    Down,
    Tab,
}

impl Open {
    pub fn placement(self) -> Placement {
        match self {
            Open::Floating => Placement::Floating,
            Open::Right => Placement::Right,
            Open::Down => Placement::Down,
            Open::Tab => Placement::Tab,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Need {
    /// The pane is in a git repository (local panes).
    Git,
    /// The pane is connected to a server.
    Server,
    /// The pane is on this machine.
    Local,
    /// A Kubernetes context is known for the pane.
    Kubernetes,
    /// The pane is in a Terraform directory.
    Terraform,
    /// An AWS profile is known for the pane.
    Aws,
    /// Some text is selected in the pane.
    Selection,
}

/// What kraal knows about the focused pane when a tool starts.
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    /// The pane's directory on this machine (local panes).
    pub cwd: Option<PathBuf>,
    /// The server the pane is connected to.
    pub server: Option<Server>,
    /// The pane's directory on that server, when its shell reports it.
    pub remote_cwd: Option<PathBuf>,
    pub git: Option<GitInfo>,
    pub kube: Option<KubeContext>,
    /// `KUBECONFIG` of the pane's Kubernetes tools ("" for the default file).
    pub kubeconfig: Option<String>,
    pub terraform: Option<String>,
    pub aws_profile: Option<String>,
    /// The pane's AWS tools used the default profile (so an inherited `AWS_PROFILE` must go).
    pub aws_default: bool,
    pub selection: Option<String>,
    /// Why the pane is production, if it is.
    pub production: Option<String>,
    /// More variables for the command (a runbook's parameters).
    pub extra: Vec<(String, String)>,
    /// Variables whose values never go into a command line, an event or the saved session
    /// (secret runbook parameters).
    pub secrets: Vec<(String, String)>,
}

/// Carries the secret variables to the pane's command, as `NAME='value'` lines.
const SECRET_VAR: &str = "KRAAL_SECRET_ENV";

impl ToolContext {
    /// `web-prod-1:/srv/app` or `~/Dev/app`, for titles.
    pub fn place(&self) -> String {
        let dir = |p: &PathBuf| crate::paths::shorten_home(p);
        match (&self.server, &self.remote_cwd, &self.cwd) {
            (Some(s), Some(d), _) => format!("{}:{}", s.name, d.display()),
            (Some(s), None, _) => s.name.clone(),
            (None, _, Some(d)) => dir(d),
            (None, _, None) => "this machine".into(),
        }
    }

    /// Variables the tool's command can use: `$KRAAL_CWD`, `$KRAAL_SERVER`, `$KRAAL_SELECTION`…
    /// Unknown ones are empty.
    pub fn variables(&self, remote: bool) -> Vec<(String, String)> {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        let cwd = if remote { path(&self.remote_cwd) } else { path(&self.cwd) };
        let mut selection = self.selection.clone().unwrap_or_default();
        if selection.len() > 4096 {
            let mut end = 4096;
            while !selection.is_char_boundary(end) {
                end -= 1;
            }
            selection.truncate(end);
        }
        let vars = [
            ("KRAAL_CWD", cwd),
            ("KRAAL_SERVER", self.server.as_ref().map(|s| s.name.clone()).unwrap_or_default()),
            ("KRAAL_HOST", self.server.as_ref().map(|s| s.destination()).unwrap_or_default()),
            ("KRAAL_REPO", self.git.as_ref().map(|g| g.root.display().to_string()).unwrap_or_default()),
            ("KRAAL_BRANCH", self.git.as_ref().map(|g| g.branch.clone()).unwrap_or_default()),
            ("KRAAL_KUBE_CONTEXT", self.kube.as_ref().map(|k| k.context.clone()).unwrap_or_default()),
            ("KRAAL_KUBE_NAMESPACE", self.kube.as_ref().and_then(|k| k.namespace.clone()).unwrap_or_default()),
            ("KRAAL_TF_WORKSPACE", self.terraform.clone().unwrap_or_default()),
            ("KRAAL_AWS_PROFILE", self.aws_profile.clone().unwrap_or_default()),
            ("KRAAL_SELECTION", selection),
            ("KRAAL_PRODUCTION", if self.production.is_some() { "1".into() } else { String::new() }),
        ];
        let mut out: Vec<(String, String)> = vars.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        out.extend(self.extra.iter().cloned());
        out
    }
}

impl ToolDef {
    /// Why the tool cannot run for this pane, if it cannot.
    pub fn missing(&self, ctx: &ToolContext) -> Option<&'static str> {
        for need in &self.when {
            let reason = match need {
                Need::Git if ctx.git.is_none() => "not in a git repository",
                Need::Server if ctx.server.is_none() => "this pane is not on a server",
                Need::Local if ctx.server.is_some() => "only for panes on this machine",
                Need::Kubernetes if ctx.kube.is_none() => "no Kubernetes context seen in this pane",
                Need::Terraform if ctx.terraform.is_none() => "not in a Terraform directory",
                Need::Aws if ctx.aws_profile.is_none() => "no AWS profile seen in this pane",
                Need::Selection if ctx.selection.as_deref().is_none_or(|s| s.trim().is_empty()) => {
                    "select some text first"
                }
                _ => continue,
            };
            return Some(reason);
        }
        None
    }

    /// Whether the tool runs on the pane's server.
    pub fn runs_remote(&self, ctx: &ToolContext) -> bool {
        self.on == Where::Pane && ctx.server.is_some()
    }

    /// The program a command line starts with (`k9s` in `k9s --context "$X"`, a quoted path), to
    /// check it is installed; `None` when the line starts with shell syntax, a builtin or a
    /// relative path, which only the shell can judge.
    pub fn program(&self) -> Option<String> {
        let line = self.command.trim_start();
        let first = match line.chars().next()? {
            q @ ('\'' | '"') => {
                let end = 1 + line[1..].find(q)?;
                let word = &line[1..end];
                // The word goes on after the quote (`'/opt/My Tools'/tool`): only the shell
                // knows what it is.
                if line[end + 1..].chars().next().is_some_and(|c| !c.is_whitespace()) {
                    return None;
                }
                // Inside double quotes the shell still expands.
                (q == '\'' || !word.contains(['$', '`', '\\'])).then_some(word)?
            }
            _ => {
                let word = line.split_whitespace().next()?;
                if word
                    .contains(['|', '&', ';', '$', '`', '(', ')', '<', '>', '=', '{', '\\', '\'', '"', '*', '?', '['])
                {
                    return None;
                }
                word
            }
        };
        if SHELL_WORDS.contains(&first) || (first.contains('/') && !first.starts_with('/')) {
            return None;
        }
        Some(first.to_string())
    }

    /// Whether the tool can be offered here: enabled, and its program installed when it runs
    /// on this machine (on a server, kraal cannot know without connecting).
    pub fn installed(&self, ctx: &ToolContext) -> bool {
        self.runs_remote(ctx) || self.program().is_none_or(|p| crate::transfer::have_program(&p))
    }

    /// The directory a tool must run in exists (a tool never runs somewhere else than asked).
    pub fn check_cwd(spec: &PaneSpec) -> anyhow::Result<()> {
        match &spec.cwd {
            Some(d) if !d.is_dir() => anyhow::bail!("{} does not exist any more", d.display()),
            _ => Ok(()),
        }
    }

    /// The pane that runs the tool. `ssh_command` builds `ssh … host <remote>` for a server.
    pub fn spec(&self, ctx: &ToolContext, ssh_command: &dyn Fn(&Server, &[String]) -> Vec<String>) -> PaneSpec {
        let mut spec = PaneSpec { close_on_exit: !self.hold, keep_on_failure: true, ..Default::default() };
        if self.runs_remote(ctx) {
            let server = ctx.server.as_ref().expect("remote tools have a server");
            let mut script = String::new();
            if let Some(d) = &ctx.remote_cwd {
                // Never run somewhere else than asked: a directory that is gone is an error.
                let d = shell::quote(&d.display().to_string());
                script.push_str(&format!("cd {d} || {{ printf 'kraal: cannot go to %s\\n' {d} >&2; exit 1; }}; "));
            }
            script.push_str("env");
            for (k, v) in ctx.variables(true) {
                script.push_str(&format!(" {k}={}", shell::quote(&v)));
            }
            script.push_str(&format!(" sh -c {}", shell::quote(&self.command)));
            spec.command = if ctx.secrets.is_empty() {
                // Through sh: the login shell on the server may not be a POSIX one.
                ssh_command(server, &[shell::sh_c(&script)])
            } else {
                // The values go through ssh's input into a private file on the server, which the
                // command reads and deletes: never in an argument list there or here.
                let mut upload = ssh_command(server, &[]);
                // The file goes away even if the command never starts (a second connection that
                // fails): a sleeper deletes it after two minutes; the command deletes it at once.
                let receive = "umask 077; f=$(mktemp) || exit 1; \
                     (sleep 120; rm -f \"$f\") </dev/null >/dev/null 2>&1 & \
                     if ! cat > \"$f\"; then rm -f \"$f\"; exit 1; fi; echo \"$f\"";
                upload.push(format!("sh -c {}", shell::quote(receive)));
                let inner = format!("set -a; . \"$1\"; rm -f \"$1\"; set +a; {script}");
                let main = ssh_command(server, &[format!("sh -c {} kraal", shell::quote(&inner))]);
                let wrapper = format!(
                    "{}f=$(printf '%s' \"${SECRET_VAR}\" | {}) || exit 1; unset {SECRET_VAR}; \
                     case $f in *[!A-Za-z0-9._/-]*|[!/]*|'') echo 'kraal: could not hand over the secret values' >&2; exit 1;; esac; \
                     exec {} \"$f\"",
                    secrets_guard(),
                    shell::join(&upload),
                    shell::join(&main)
                );
                spec.secret_env.0.push((SECRET_VAR.into(), secret_lines(&ctx.secrets)));
                vec!["sh".into(), "-c".into(), wrapper]
            };
            spec.server = Some(server.name.clone());
            // The connection the tool was given (the pane's own: its account, its jump host),
            // not what the inventory entry of that name says.
            spec.connection = Some(server.clone());
        } else {
            spec.command = if ctx.secrets.is_empty() {
                vec!["sh".into(), "-c".into(), self.command.clone()]
            } else {
                spec.secret_env.0.push((SECRET_VAR.into(), secret_lines(&ctx.secrets)));
                let load = format!("{}set -a; eval \"${SECRET_VAR}\"; set +a; unset {SECRET_VAR}; ", secrets_guard());
                vec!["sh".into(), "-c".into(), format!("{load}{}", self.command)]
            };
            spec.cwd = if ctx.server.is_none() { ctx.cwd.clone() } else { None };
            spec.env = ctx.variables(false);
            // Local tools act on the pane's cluster and cloud account: the ones its commands used,
            // not whatever kraal itself inherited. Those a shell on a server reported are named
            // in the variables only: its kubeconfig and profiles are files on the server.
            match ctx.kubeconfig.as_deref() {
                Some("") => spec.env_remove.push("KUBECONFIG".into()),
                Some(k) => spec.env.push(("KUBECONFIG".into(), k.to_string())),
                None => {}
            }
            match &ctx.aws_profile {
                Some(p) if ctx.server.is_none() => spec.env.push(("AWS_PROFILE".into(), p.clone())),
                None if ctx.aws_default => spec.env_remove.extend(["AWS_PROFILE".into(), "AWS_DEFAULT_PROFILE".into()]),
                _ => {}
            }
        }
        spec
    }
}

/// Built-ins and reserved words of the usual shells a command line may start with: no program
/// to look for (`unalias -a; make` needs no `unalias` on the PATH).
const SHELL_WORDS: &[&str] = &[
    // Reserved words.
    "!",
    "{",
    "}",
    "[[",
    "]]",
    "case",
    "coproc",
    "do",
    "done",
    "elif",
    "else",
    "esac",
    "fi",
    "for",
    "function",
    "if",
    "in",
    "select",
    "then",
    "time",
    "until",
    "while",
    // POSIX special and regular built-ins.
    ".",
    ":",
    "alias",
    "bg",
    "break",
    "cd",
    "command",
    "continue",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "jobs",
    "kill",
    "newgrp",
    "pwd",
    "read",
    "readonly",
    "return",
    "set",
    "shift",
    "test",
    "[",
    "times",
    "trap",
    "true",
    "type",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
    // Built-ins of bash and zsh.
    "bind",
    "builtin",
    "caller",
    "compgen",
    "complete",
    "compopt",
    "declare",
    "dirs",
    "disown",
    "echo",
    "enable",
    "help",
    "history",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "printf",
    "pushd",
    "readarray",
    "shopt",
    "source",
    "suspend",
    "typeset",
    "autoload",
    "bindkey",
    "emulate",
    "noglob",
    "print",
    "setopt",
    "unsetopt",
    "whence",
    "where",
    "which",
    "zmodload",
];

fn secret_lines(secrets: &[(String, String)]) -> String {
    secrets.iter().map(|(k, v)| format!("{k}={}\n", shell::quote(v))).collect()
}

/// Stops a command whose secret values are gone (it was restored after a restart: they are not
/// kept), rather than running it without them.
fn secrets_guard() -> String {
    format!(
        "[ -n \"${{{SECRET_VAR}+x}}\" ] || {{ echo 'kraal: secret values are not kept; start the runbook again' >&2; exit 1; }}; "
    )
}

/// Problems in the `[tools]` section, as warnings.
pub fn check(tools: &BTreeMap<String, ToolDef>) -> Vec<String> {
    let mut out = Vec::new();
    for (name, t) in tools {
        if t.command.trim().is_empty() && t.enabled {
            out.push(format!("tools.{name}: empty command (set enabled = false to hide a tool)"));
        }
        if t.when.contains(&Need::Server) && t.when.contains(&Need::Local) {
            out.push(format!("tools.{name}: when = [\"server\", \"local\"] can never be met"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script inside `sh -c '<script>'`.
    fn unwrap_sh_c(line: &str) -> String {
        let quoted = line.strip_prefix("sh -c '").and_then(|q| q.strip_suffix('\'')).expect("an sh -c line");
        quoted.replace("'\\''", "'")
    }

    fn tool(toml_text: &str) -> ToolDef {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn needs_and_programs() {
        let t = tool("command = \"lazygit\"\nwhen = [\"git\"]\non = \"local\"");
        let mut ctx = ToolContext::default();
        assert_eq!(t.missing(&ctx), Some("not in a git repository"));
        ctx.git = crate::git::parse_status("# branch.oid abc\n# branch.head main\n");
        assert_eq!(t.missing(&ctx), None);
        assert_eq!(t.program().as_deref(), Some("lazygit"));
        assert_eq!(tool("command = \"htop || top\"").program().as_deref(), Some("htop"));
        assert_eq!(tool("command = \"k9s --context $X\"").program().as_deref(), Some("k9s"));
        assert_eq!(tool("command = \"FOO=1 tool\"").program(), None);
        // A quoted path is a program; builtins and relative paths are for the shell to judge.
        let quoted = tool("command = \"'/opt/My Tools/tool' --x\"");
        assert_eq!(quoted.program().as_deref(), Some("/opt/My Tools/tool"));
        assert_eq!(tool("command = '\"/opt/$X/tool\"'").program(), None);
        assert_eq!(tool("command = \"cd /srv && make\"").program(), None);
        assert_eq!(tool("command = \"./bin/tool\"").program(), None);
        assert_eq!(tool("command = \"'/opt/My Tools'/tool\"").program(), None, "the word goes on after the quote");
        assert_eq!(tool("command = \": && make\"").program(), None, "a builtin");
        for builtin in ["unalias -a; make", "pushd /srv && make", "declare -x A=1; make", "{ make; }"] {
            assert_eq!(tool(&format!("command = {builtin:?}")).program(), None, "{builtin}");
        }
        let sel = tool("command = \"git show $KRAAL_SELECTION\"\nwhen = [\"selection\"]");
        ctx.selection = Some("  ".into());
        assert!(sel.missing(&ctx).is_some());
        assert!(toml::from_str::<ToolDef>("command = \"x\"\nwhen = [\"nope\"]").is_err());
        assert!(toml::from_str::<ToolDef>("command = \"x\"\ntypo = 1").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn secrets_reach_the_command_but_no_argument_list() {
        let dir = std::env::temp_dir().join(format!("kraal-secrets-{}", std::process::id()));
        let tmp = dir.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        // A stand-in for ssh: it runs the remote command line as a login shell would.
        let fake = dir.join("ssh");
        crate::testing::write_script(&fake, "#!/bin/sh\nshift\nexec sh -c \"$*\"\n");
        let ssh = |s: &Server, remote: &[String]| {
            let mut v = vec![fake.display().to_string(), s.destination()];
            v.extend(remote.iter().cloned());
            v
        };
        let secret = "it's a \"$hush\" `x`";
        let out = dir.join("out");
        let t = tool(&format!("command = \"printf '%s' \\\"$TOKEN\\\" > {}\"", out.display()));
        let run = |spec: &crate::pane::PaneSpec, with_secrets: bool| {
            let mut c = std::process::Command::new(&spec.command[0]);
            c.args(&spec.command[1..]).env("TMPDIR", &tmp).envs(spec.env.iter().cloned());
            if with_secrets {
                c.envs(spec.secret_env.0.iter().cloned());
            }
            c.output().unwrap()
        };
        let remote_ctx = ToolContext {
            server: Some(Server { name: "web".into(), host: "web.lan".into(), ..Default::default() }),
            secrets: vec![("TOKEN".into(), secret.into())],
            ..Default::default()
        };
        let local_ctx = ToolContext { secrets: vec![("TOKEN".into(), secret.into())], ..Default::default() };
        for ctx in [remote_ctx, local_ctx] {
            let _ = std::fs::remove_file(&out);
            let spec = t.spec(&ctx, &ssh);
            assert!(!spec.command.iter().any(|w| w.contains("hush")), "{:?}", spec.command);
            assert!(!format!("{spec:?}").contains("hush"));
            assert!(!serde_json::to_string(&spec).unwrap().contains("hush"));
            let done = run(&spec, true);
            assert!(done.status.success(), "{done:?}");
            assert_eq!(std::fs::read_to_string(&out).unwrap(), secret);
            assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0, "the uploaded values are deleted");
            // Restored without its values (they are not kept), it refuses to run.
            let _ = std::fs::remove_file(&out);
            let bare = run(&spec, false);
            assert!(!bare.status.success() && !out.exists());
            assert!(String::from_utf8_lossy(&bare.stderr).contains("start the runbook again"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn local_and_remote_specs() {
        let ssh = |s: &Server, remote: &[String]| {
            let mut v = vec!["ssh".to_string(), "-t".into(), s.destination()];
            v.extend(remote.iter().cloned());
            v
        };
        let t = tool("command = \"journalctl -f\"");
        let mut ctx = ToolContext {
            cwd: Some("/home/me/app".into()),
            kubeconfig: Some("/home/me/.kube/prod".into()),
            selection: Some("it's".into()),
            ..Default::default()
        };
        let local = t.spec(&ctx, &ssh);
        assert_eq!(local.command, vec!["sh", "-c", "journalctl -f"]);
        assert_eq!(local.cwd, Some(PathBuf::from("/home/me/app")));
        assert!(local.close_on_exit && local.keep_on_failure);
        assert!(local.env.contains(&("KUBECONFIG".into(), "/home/me/.kube/prod".into())));
        assert!(local.env.contains(&("KRAAL_SELECTION".into(), "it's".into())));

        ctx.server =
            Some(Server { name: "web".into(), host: "web.lan".into(), user: Some("ops".into()), ..Default::default() });
        ctx.remote_cwd = Some("/srv/my app".into());
        let remote = t.spec(&ctx, &ssh);
        assert_eq!(remote.server.as_deref(), Some("web"));
        assert_eq!(&remote.command[..3], &["ssh", "-t", "ops@web.lan"]);
        let script = &unwrap_sh_c(&remote.command[3]);
        assert!(
            script.starts_with(
                "cd '/srv/my app' || { printf 'kraal: cannot go to %s\\n' '/srv/my app' >&2; exit 1; }; env KRAAL_CWD='/srv/my app' KRAAL_SERVER=web"
            ),
            "{script}"
        );
        assert!(script.contains("KRAAL_SELECTION='it'\\''s'"), "{script}");
        assert!(script.ends_with(" sh -c 'journalctl -f'"), "{script}");
        assert!(remote.env.is_empty(), "local variables do not leak into the ssh process");
        // A directory name is data, even in the error message.
        ctx.remote_cwd = Some("/nowhere/$(echo INJECTED)".into());
        let script = unwrap_sh_c(&t.spec(&ctx, &ssh).command[3]);
        let cd_part = script.split("; env").next().unwrap().to_string();
        let out = std::process::Command::new("sh").arg("-c").arg(&cd_part).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success()
                && err.contains("$(echo INJECTED)")
                && !err.contains("cannot go to /nowhere/INJECTED"),
            "{err}"
        );

        let local_only = tool("command = \"k9s\"\non = \"local\"");
        let spec = local_only.spec(&ctx, &ssh);
        assert_eq!(spec.command[0], "sh");
        assert_eq!(spec.cwd, None, "an SSH pane's remote directory is not a local one");

        // What a shell on the server reported: named in the variables, here and there, but the
        // profile is the server's, not one to use here.
        ctx.kubeconfig = None;
        ctx.kube = Some(KubeContext { context: "prod-eks".into(), namespace: Some("payments".into()) });
        ctx.aws_profile = Some("ops".into());
        let here = local_only.spec(&ctx, &ssh);
        assert!(here.env.contains(&("KRAAL_KUBE_CONTEXT".into(), "prod-eks".into())), "{:?}", here.env);
        assert!(here.env.contains(&("KRAAL_AWS_PROFILE".into(), "ops".into())), "{:?}", here.env);
        assert!(!here.env.iter().any(|(k, _)| k == "AWS_PROFILE" || k == "KUBECONFIG"), "{:?}", here.env);
        assert!(here.env_remove.is_empty(), "{:?}", here.env_remove);
        let script = unwrap_sh_c(&t.spec(&ctx, &ssh).command[3]);
        assert!(script.contains("KRAAL_KUBE_CONTEXT=prod-eks KRAAL_KUBE_NAMESPACE=payments"), "{script}");
        // On a local pane, the profile its tools used is the one tools use.
        ctx.server = None;
        assert!(local_only.spec(&ctx, &ssh).env.contains(&("AWS_PROFILE".into(), "ops".into())));
    }
}
