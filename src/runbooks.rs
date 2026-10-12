//! Runbooks: procedures kept as files, in `~/.config/kraal/runbooks/` and in a repository's
//! `.kraal/runbooks/`.
//!
//! - A TOML runbook has steps, parameters and a target (this machine, the pane, or servers).
//!   kraal shows the plan, runs the steps server by server, stops at the first failure and keeps
//!   a log of what ran where.
//! - A Markdown runbook is a document: its shell blocks run one by one in a pane, as you read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::paths;
use crate::shell;
use crate::worktree_setup::visible_lines;

/// Where a runbook comes from: the user's own run as they are, a repository's only once the
/// user approved its content ([`trusted`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `~/.config/kraal/runbooks`.
    User,
    /// `<repository>/.kraal/runbooks`: the repository, where it really is ([`load`]).
    Repo(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Runbook {
    /// The file name without its extension; `<repo>/<name>` for a repository's runbook named
    /// like one of the user's ([`all`]).
    pub name: String,
    /// The file: a repository's in that repository's real directory ([`load`]).
    pub path: PathBuf,
    pub source: Source,
    /// The file as read, to know whether a repository runbook changed since it was trusted.
    pub content: String,
    pub body: Body,
}

#[derive(Debug, Clone)]
pub enum Body {
    /// A `.toml` runbook.
    Steps(Box<Steps>),
    /// A `.md` runbook: its first heading, and its shell blocks.
    Markdown { title: String, blocks: Vec<Block> },
}

impl Runbook {
    pub fn description(&self) -> String {
        match &self.body {
            Body::Steps(s) => s.description.clone(),
            Body::Markdown { title, blocks } => {
                format!("{title} · {} step{}", blocks.len(), if blocks.len() == 1 { "" } else { "s" })
            }
        }
    }
}

impl Source {
    /// Text of a runbook from there (its file, its name, a step) as the command line prints it:
    /// the user's own as it is, a repository's with what a terminal would act on or hide
    /// written out ([`visible_lines`]), so that what is read is what gets approved.
    pub fn shown(&self, text: &str) -> String {
        match self {
            Source::User => text.to_string(),
            Source::Repo(_) => visible_lines(text),
        }
    }
}

/// Where the steps of a TOML runbook run (`on`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum On {
    /// This machine, in the repository (repository runbooks) or the pane's directory.
    #[default]
    Local,
    /// Where the focused pane is: its server, or here in its directory.
    Pane,
    /// Servers or a group, chosen when it runs.
    Server,
}

/// A TOML runbook, as its file says. A key kraal does not know is refused, where the
/// configuration only warns: a runbook says what runs and how, and running it without what a
/// key asked for (`stop_on_failure`, `timeout` or `secret`, misspelt) is worse than not running it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Steps {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub on: On,
    /// Servers run at the same time; 0 runs them all at once.
    #[serde(default = "one")]
    pub rolling: usize,
    /// Whether the first failure stops the run: the rest of its target's steps ([`script`]) and
    /// the targets still waiting. Without it, every step and every target runs.
    #[serde(default = "yes")]
    pub stop_on_failure: bool,
    /// Directory for local runs (`~` and paths relative to the repository work).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Shell the steps run in.
    #[serde(default = "default_shell")]
    pub shell: String,
    /// Longest a target may run (`90`, `30s`, `10m`, `2h`); it is stopped and counts as failed.
    #[serde(default)]
    pub timeout: Option<String>,
    /// Shell code, run in order.
    pub steps: Vec<String>,
    /// Asked for before the plan is shown, given to the steps as variables.
    #[serde(default, rename = "param")]
    pub params: Vec<Param>,
}

fn one() -> usize {
    1
}
fn yes() -> bool {
    true
}
fn default_shell() -> String {
    "sh".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    /// The variable the steps read (`$VERSION`).
    pub name: String,
    /// The question asked; the name when there is none.
    #[serde(default)]
    pub prompt: Option<String>,
    /// May use the pane's `$KRAAL_*` variables (`$KRAAL_BRANCH`).
    #[serde(default)]
    pub default: Option<String>,
    /// The values to choose from. With neither these nor a `choices_command`, any value goes.
    #[serde(default)]
    pub choices: Vec<String>,
    /// A command whose output lines are the choices (`git tag --sort=-v:refname | head`).
    #[serde(default)]
    pub choices_command: Option<String>,
    /// A password or token: given to the steps, never shown in the plan, the log or events.
    #[serde(default)]
    pub secret: bool,
}

impl Param {
    pub fn prompt(&self) -> String {
        self.prompt.clone().unwrap_or_else(|| self.name.clone())
    }

    /// Refuse a value the question about this parameter does not offer. With `choices` (the
    /// file's, or what its command listed) the question is a list, which takes one of them and
    /// nothing else; without any, it takes whatever is typed.
    pub fn accept(&self, value: &str, choices: &[String]) -> Result<()> {
        if choices.is_empty() || choices.iter().any(|choice| choice == value) {
            return Ok(());
        }
        let (name, value) = (&self.name, shown_value(Some(self), value));
        bail!("{name}={value} is not one of its choices: {}", choices.join(", "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The heading the block is under.
    pub heading: String,
    /// The commands, without a console transcript's prompts and output.
    pub code: String,
}

fn valid_var(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with("KRAAL_")
}

fn parse_steps(text: &str) -> Result<Steps> {
    let s: Steps = toml::from_str(text)?;
    if s.steps.iter().all(|st| st.trim().is_empty()) {
        bail!("no steps");
    }
    let mut seen = std::collections::HashSet::new();
    for p in &s.params {
        if !valid_var(&p.name) {
            bail!("param {:?}: use letters, digits and _ (not KRAAL_…), it becomes ${}", p.name, p.name);
        }
        if !seen.insert(p.name.clone()) {
            bail!("param {:?} is defined twice", p.name);
        }
    }
    if s.shell.is_empty() || !s.shell.chars().all(|c| c.is_ascii_alphanumeric() || "._/+-".contains(c)) {
        bail!("shell must be a program name or path, like \"bash\"");
    }
    if let Some(t) = &s.timeout {
        parse_duration(t).with_context(|| format!("timeout {t:?}: use 90, 30s, 10m or 2h"))?;
    }
    Ok(s)
}

impl Steps {
    pub fn timeout(&self) -> Option<std::time::Duration> {
        self.timeout.as_deref().and_then(parse_duration)
    }
}

/// `90`, `30s`, `10m`, `2h`.
fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "s"),
    };
    let n: u64 = num.parse().ok().filter(|n| *n > 0)?;
    let secs = match unit {
        "s" => Some(n),
        "m" => n.checked_mul(60),
        "h" => n.checked_mul(3600),
        _ => None,
    }?;
    Some(std::time::Duration::from_secs(secs))
}

/// A parameter value as it may be shown: secrets are masked.
pub fn shown_value(p: Option<&Param>, value: &str) -> String {
    if p.is_some_and(|p| p.secret) { "••••".into() } else { value.to_string() }
}

/// The title (first heading) and the shell blocks of a Markdown runbook.
fn parse_markdown(text: &str) -> (String, Vec<Block>) {
    use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};
    let mut title = String::new();
    let mut heading = String::new();
    let mut in_heading: Option<HeadingLevel> = None;
    let mut code: Option<String> = None;
    let mut transcript = false;
    let mut blocks = Vec::new();
    for ev in Parser::new(text) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = Some(level);
                heading.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if title.is_empty() {
                    title = heading.clone();
                }
                in_heading = None;
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) => {
                let lang = lang.split([' ', ',', '{']).next().unwrap_or("").to_ascii_lowercase();
                if matches!(lang.as_str(), "sh" | "bash" | "shell" | "zsh" | "console" | "shell-session") {
                    code = Some(String::new());
                    // Only transcripts have `$ ` prompts to strip; in a script, `$ ` is code.
                    transcript = matches!(lang.as_str(), "console" | "shell-session");
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(c) = code.take() {
                    let c = if transcript { console_commands(&c) } else { c.trim_end().to_string() };
                    if !c.trim().is_empty() {
                        blocks.push(Block { heading: heading.clone(), code: c });
                    }
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(c) = &mut code {
                    c.push_str(&t);
                } else if in_heading.is_some() {
                    heading.push_str(&t);
                }
            }
            _ => {}
        }
    }
    (title, blocks)
}

/// Where a runbook file comes from, from its path.
pub fn source_of(path: &Path) -> Source {
    let dir = path.parent();
    if dir.is_some_and(|d| d.ends_with(".kraal/runbooks"))
        && let Some(root) = dir.and_then(Path::parent).and_then(Path::parent)
    {
        return Source::Repo(root.to_path_buf());
    }
    Source::User
}

/// The commands of a console transcript: `$ ` lines, with the lines they continue (`\` at the
/// end); the rest is output.
fn console_commands(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut continuing = false;
    for line in text.lines() {
        if let Some(cmd) = line.strip_prefix("$ ") {
            out.push(cmd);
            continuing = cmd.ends_with('\\');
        } else if continuing {
            let cont = line.strip_prefix("> ").unwrap_or(line);
            out.push(cont);
            continuing = cont.ends_with('\\');
        }
    }
    out.join("\n")
}

pub fn user_dir() -> PathBuf {
    paths::config_dir().join("runbooks")
}

pub fn repo_dir(root: &Path) -> PathBuf {
    root.join(".kraal").join("runbooks")
}

/// Runbooks are short files: a repository can hold anything under their name (a link to a device,
/// a FIFO, a huge file), which must not hang or exhaust the server that lists them.
const RUNBOOK_MAX: u64 = 1 << 20;

/// The runbook in the file at `path`. A repository's is known by its place in the real directory
/// of its repository, however the way there was spelled (through a link, `/tmp` on macOS): what
/// is approved, read and run is one file in one place, which a link that changes later cannot
/// turn into another.
pub fn load(path: &Path, source: Source) -> Result<Runbook> {
    let (path, source) = match (source, path.file_name()) {
        (Source::Repo(root), Some(file)) => {
            let root = crate::git::real_path(&root);
            (repo_dir(&root).join(file), Source::Repo(root))
        }
        (source, _) => (path.to_path_buf(), source),
    };
    let path = path.as_path();
    let content = paths::read_regular_text(path, RUNBOOK_MAX).with_context(|| format!("reading {}", path.display()))?;
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let body = match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => Body::Steps(Box::new(parse_steps(&content).with_context(|| format!("in {}", path.display()))?)),
        Some("md") => {
            let (title, blocks) = parse_markdown(&content);
            Body::Markdown { title, blocks }
        }
        _ => bail!("{}: runbooks are .toml or .md files", path.display()),
    };
    Ok(Runbook { name, path: path.to_path_buf(), source, content, body })
}

/// Runbooks of a directory, sorted by name; unreadable ones as errors.
pub fn load_dir(dir: &Path, source: &Source) -> Vec<Result<Runbook>> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "toml" || e == "md")).collect();
    paths.sort();
    paths.into_iter().map(|p| load(&p, source.clone())).collect()
}

/// The user's runbooks, then those of the repository `repo` (when given). A repository runbook
/// with the name of one of yours is listed as `<repo>/<name>`.
pub fn all(repo: Option<&Path>) -> (Vec<Runbook>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for r in load_dir(&user_dir(), &Source::User) {
        match r {
            Ok(r) => out.push(r),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    if let Some(root) = repo {
        let repo_name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let source = Source::Repo(root.to_path_buf());
        for r in load_dir(&repo_dir(root), &source) {
            match r {
                Ok(mut r) => {
                    if out.iter().any(|o: &Runbook| o.name == r.name) {
                        r.name = format!("{repo_name}/{}", r.name);
                    }
                    out.push(r);
                }
                // The error quotes the file.
                Err(e) => errors.push(source.shown(&format!("{e:#}"))),
            }
        }
    }
    (out, errors)
}

/// Replace `$KRAAL_X` and `${KRAAL_X}` with the pane's values (in parameter defaults).
pub fn expand_vars(s: &str, vars: &[(String, String)]) -> String {
    let mut out = s.to_string();
    // Longest names first, so $KRAAL_KUBE_CONTEXT is not read as $KRAAL_KUBE + _CONTEXT.
    let mut vars: Vec<&(String, String)> = vars.iter().collect();
    vars.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    for (k, v) in vars {
        out = out.replace(&format!("${{{k}}}"), v).replace(&format!("${k}"), v);
    }
    out
}

/// The shell script that runs `steps` in order, announcing each one. Its exit code is the
/// first failing step's.
///
/// - With `stop_on_failure`, the steps run in one shell with `set -e` (and `pipefail` where the
///   shell has it): the first command that fails ends the run, as in a script, and steps share
///   the shell, so a `cd` or a variable carries on to the next.
/// - Without it, each step runs in its own subshell with `set -e`: a failing command ends its
///   step, the next step runs anyway, and nothing carries over between steps.
pub fn script(steps: &[String], stop_on_failure: bool) -> String {
    let steps: Vec<&String> = steps.iter().filter(|s| !s.trim().is_empty()).collect();
    let n = steps.len();
    let header = |i: usize, step: &str| {
        format!(
            "__kraal_step={i}\nprintf '\\n\\033[1;36m▶ %s/%s\\033[0m %s\\n' {i} {n} {}\n",
            shell::quote(&crate::text::first_line(step, 100))
        )
    };
    const FAILED: &str = "printf '\\n\\033[1;31m✗ step %s failed (exit %s)\\033[0m\\n'";
    const DONE: &str = "printf '\\n\\033[1;32m✓ done\\033[0m\\n'";
    let mut s = String::new();
    if stop_on_failure {
        s.push_str(&format!(
            "__kraal_end() {{\n__kraal_s=$?\nif [ \"$__kraal_s\" = 0 ]; then {DONE}; \
             else {FAILED} \"$__kraal_step\" \"$__kraal_s\"; fi\n}}\n\
             trap __kraal_end EXIT\n__kraal_step=0\nset -e\n(set -o pipefail) 2>/dev/null && set -o pipefail\n"
        ));
        for (i, step) in steps.iter().enumerate() {
            s.push_str(&header(i + 1, step));
            s.push_str(step.trim_end());
            s.push('\n');
        }
    } else {
        s.push_str("__kraal_fail=0\n");
        for (i, step) in steps.iter().enumerate() {
            s.push_str(&header(i + 1, step));
            // A standalone subshell: `set -e` would be ignored on the left of `||`.
            s.push_str(&format!(
                "(\nset -e\n(set -o pipefail) 2>/dev/null && set -o pipefail\n{}\n)\n__kraal_s=$?\n\
                 if [ \"$__kraal_s\" != 0 ]; then {FAILED} {} \"$__kraal_s\"; [ \"$__kraal_fail\" = 0 ] && __kraal_fail=$__kraal_s; fi\n",
                step.trim_end(),
                i + 1
            ));
        }
        s.push_str(&format!(
            "if [ \"$__kraal_fail\" = 0 ]; then {DONE}; \
             else printf '\\n\\033[1;31m✗ some steps failed\\033[0m\\n'; fi\nexit \"$__kraal_fail\"\n"
        ));
    }
    s
}

/// The command line that runs `script` in `shell`.
pub fn command(shell: &str, script: &str) -> String {
    if shell == "sh" { script.to_string() } else { format!("exec {} -c {}", shell::quote(shell), shell::quote(script)) }
}

// ---- trusting repository files ----------------------------------------------------------

/// The repository files the user approved, by path ([`approval_key`]), each with the exact
/// content approved.
fn trust_path() -> PathBuf {
    paths::state_dir().join("trusted-runbooks.json")
}

fn trust_store() -> BTreeMap<String, String> {
    std::fs::read_to_string(trust_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// What the approval of `file` (its path inside its repository: `.kraal/…`) of the repository
/// in the real directory `repo` is recorded under: one path for one file, whichever way a
/// caller came to it. What is inside the repository is not resolved: the file is that
/// repository's wherever its `.kraal` leads.
fn approval_key(repo: &Path, file: &Path) -> String {
    repo.join(file).display().to_string()
}

/// Whether the store's `key` names `file` of the repository in the real directory `repo` under
/// another spelling of the way there: what 0.3.0 recorded when it was reached through a link.
fn other_spelling(key: &str, repo: &Path, file: &Path) -> bool {
    let key = Path::new(key);
    // The directory the key gives the repository: what comes before the file's place in it.
    let spelled = key.ancestors().nth(file.components().count()).filter(|_| key.ends_with(file));
    spelled.is_some_and(|spelled| spelled != repo && crate::git::real_path(spelled) == repo)
}

/// Whether this exact content of a repository runbook was approved before.
pub fn trusted(r: &Runbook) -> bool {
    match &r.source {
        Source::User => true,
        Source::Repo(root) => trusted_content(root, file_in_repo(r, root), &r.content),
    }
}

/// Approve this content of a repository runbook.
pub fn trust(r: &Runbook) -> Result<()> {
    match &r.source {
        Source::User => Ok(()),
        Source::Repo(root) => trust_content(root, file_in_repo(r, root), &r.content),
    }
}

/// Where a runbook of the repository `root` is inside it (`.kraal/runbooks/deploy.toml`).
fn file_in_repo<'a>(r: &'a Runbook, root: &Path) -> &'a Path {
    r.path.strip_prefix(root).unwrap_or(&r.path)
}

/// Why the runbook `name` of the repository `root` does not run, and how to approve it.
pub fn not_approved(name: &str, root: &Path) -> String {
    refusal(name, root, &format!("kraal runbook show {name}"), &format!("kraal runbook trust {name}"))
}

/// Why kraal does not act on `what`, a file of the repository `root` (a runbook by its name,
/// the worktree setup), and how to approve it, with the command that shows it and the one that
/// approves it: said wherever nobody can be asked (`--yes`, no terminal, a plan run by another
/// program). `--yes` answers what a command asks about what it does; it approves nothing a
/// repository brings.
pub fn refusal(what: &str, root: &Path, show: &str, trust: &str) -> String {
    format!(
        "{what} comes from the repository {} and this version of it is not approved: read it (`{show}`), then \
         approve it (`{trust}`)",
        paths::shorten_home(root)
    )
}

/// Whether this exact content of `file` (its path inside its repository: a runbook, the
/// worktree setup) of the repository `repo` was approved before. The repository is the one in
/// the directory `repo` really is, however it is spelled: an approval never depends on the
/// way taken to the file, and never goes to another repository a link came to lead to.
pub fn trusted_content(repo: &Path, file: &Path, content: &str) -> bool {
    let (repo, store) = (crate::git::real_path(repo), trust_store());
    store.get(&approval_key(&repo, file)).is_some_and(|approved| approved == content)
        // Recorded by 0.3.0 under the spelling used then: it counts while that spelling leads
        // here, as it did for 0.3.0. Only entries of this very content are looked at.
        || store.iter().any(|(key, approved)| approved == content && other_spelling(key, &repo, file))
}

/// Approve this content of `file` of the repository `repo` ([`trusted_content`]): it runs from
/// now on without asking. The file has one approved content: what was recorded for it under
/// another spelling goes.
pub fn trust_content(repo: &Path, file: &Path, content: &str) -> Result<()> {
    let repo = crate::git::real_path(repo);
    let store_path = trust_path();
    if let Some(dir) = store_path.parent() {
        paths::ensure_private_dir(dir)?;
    }
    paths::locked(&store_path, || {
        let mut store = trust_store();
        store.retain(|key, _| !other_spelling(key, &repo, file));
        store.insert(approval_key(&repo, file), content.to_string());
        paths::write_private(&store_path, serde_json::to_string_pretty(&store)?.as_bytes())?;
        Ok(())
    })?
}

// ---- logs -------------------------------------------------------------------------------

/// `2026-10-11 14:03:22` in local time.
pub fn local_time(t: SystemTime) -> String {
    crate::text::local_datetime(crate::text::unix_seconds(t))
}

/// Where run logs go, with the ledger of the runs in progress.
fn runs_dir() -> PathBuf {
    paths::state_dir().join("runs")
}

/// Create the log of run `id` of `name`, private, never overwriting another run's.
pub fn create_log(name: &str, id: u32, header: &str) -> Result<PathBuf> {
    use std::io::Write;
    let stamp = local_time(SystemTime::now()).replace([' ', ':'], "-");
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let dir = runs_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    // Another session may have started a run with the same name and number in the same second.
    for n in 1..=100 {
        let suffix = if n == 1 { String::new() } else { format!("-{n}") };
        let path = dir.join(format!("{stamp}-{safe}-{id}{suffix}.log"));
        match opts.open(&path) {
            Ok(mut f) => {
                f.write_all(header.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
    bail!("could not find a free name for the log of {name} in {}", dir.display())
}

/// Most of one target's output a run log keeps.
const MAX_OUTPUT: u64 = 16 << 20;
/// A line longer than this is written in pieces.
const MAX_LINE: usize = 64 << 10;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Seq {
    Text,
    Escape,
    Csi,
    /// OSC, DCS and the other strings, up to BEL or ST; `true` after their ESC.
    Str(bool),
}

/// One target's output as plain text, written to a file as it comes: escape sequences dropped,
/// a line rewritten with `\r` (a progress bar) kept in its last form. Unlike the pane's screen,
/// nothing scrolled away or overwritten is lost.
pub struct OutputLog {
    file: std::fs::File,
    pub path: PathBuf,
    /// The first write that failed: what follows it is missing.
    error: Option<String>,
    /// The line being read, written once it ends.
    line: Vec<u8>,
    /// A `\r` came: the next character starts the line over.
    cr: bool,
    /// Where the output is in an escape sequence, which can span two reads.
    seq: Seq,
    /// Bytes in the file, at most [`MAX_OUTPUT`].
    written: u64,
    /// [`MAX_OUTPUT`] was reached: the rest of the output is dropped.
    cut: bool,
}

impl OutputLog {
    pub fn create(path: PathBuf) -> std::io::Result<OutputLog> {
        let file = paths::open_private_append(&path)?;
        Ok(OutputLog { file, path, error: None, line: Vec::new(), cr: false, seq: Seq::Text, written: 0, cut: false })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if self.cut {
            return;
        }
        let mut done = Vec::new();
        for &b in bytes {
            self.seq = match self.seq {
                Seq::Escape => match b {
                    b'[' => Seq::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => Seq::Str(false),
                    _ => Seq::Text,
                },
                Seq::Csi if (0x40..=0x7e).contains(&b) => Seq::Text,
                Seq::Csi => Seq::Csi,
                Seq::Str(_) if b == 0x07 => Seq::Text,
                Seq::Str(true) if b == b'\\' => Seq::Text,
                Seq::Str(_) => Seq::Str(b == 0x1b),
                Seq::Text => {
                    match b {
                        0x1b => {}
                        b'\n' => {
                            done.extend_from_slice(&self.line);
                            done.push(b'\n');
                            self.line.clear();
                            self.cr = false;
                        }
                        b'\r' => self.cr = true,
                        0x08 => {
                            self.line.pop();
                        }
                        b'\t' | 0x20.. if b != 0x7f => {
                            if std::mem::take(&mut self.cr) {
                                self.line.clear();
                            }
                            self.line.push(b);
                            if self.line.len() >= MAX_LINE {
                                done.append(&mut self.line);
                                done.push(b'\n');
                            }
                        }
                        _ => {}
                    }
                    if b == 0x1b { Seq::Escape } else { Seq::Text }
                }
            };
        }
        self.write(&done);
    }

    fn write(&mut self, data: &[u8]) {
        use std::io::Write;
        if data.is_empty() || self.cut || self.error.is_some() {
            return;
        }
        let room = MAX_OUTPUT.saturating_sub(self.written) as usize;
        let keep = &data[..data.len().min(room)];
        if let Err(e) = self.file.write_all(keep) {
            self.error = Some(e.to_string());
            return;
        }
        self.written += keep.len() as u64;
        if keep.len() < data.len() {
            let _ = self.file.write_all(format!("\n[kraal: output cut at {} MiB]\n", MAX_OUTPUT >> 20).as_bytes());
            self.cut = true;
        }
    }

    /// Write what is left of the last line. The file then holds the output (up to the size
    /// limit); a failed write is noted at its end and returned.
    pub fn finish(mut self) -> (PathBuf, Option<String>) {
        use std::io::Write;
        let mut last = std::mem::take(&mut self.line);
        if !last.is_empty() {
            last.push(b'\n');
            self.write(&last);
        }
        if let Some(e) = &self.error {
            let _ = self.file.write_all(format!("\n[kraal: output incomplete: {e}]\n").as_bytes());
        }
        (self.path, self.error)
    }
}

/// Append `header` to the run log, then a finished target's output, deleting its file.
fn fold_output(log: &Path, header: &str, output: Option<&Path>) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = paths::open_private_append(log)?;
    f.write_all(header.as_bytes())?;
    if let Some(output) = output {
        let mut out = std::fs::File::open(output)?;
        std::io::copy(&mut out, &mut f)?;
        f.sync_data()?;
        // Only once it is in the log.
        let _ = std::fs::remove_file(output);
    }
    Ok(())
}

/// What the run log writer does, in the order asked.
pub enum LogJob {
    /// Text for a run log.
    Append(PathBuf, String),
    /// A finished target: its header, then its output file, moved into the log.
    Fold { log: PathBuf, header: String, output: PathBuf },
    /// The runs a session has in progress, for its ledger ([`save_ledger`]).
    Ledger(String, Vec<LedgerRun>),
    /// Write the ledger and say whether it worked.
    LedgerSync(String, Vec<LedgerRun>, Box<dyn FnOnce(Result<(), String>) + Send>),
    /// Everything asked before is written: say so.
    Flush(crossbeam_channel::Sender<()>),
    /// Run this once everything asked before is written.
    Then(Box<dyn FnOnce() + Send>),
}

/// The thread that writes run logs and the ledger, in order, off the server's event loop;
/// failures are passed to `warn`.
pub fn spawn_log_writer(warn: impl Fn(String) + Send + 'static) -> std::io::Result<crossbeam_channel::Sender<LogJob>> {
    let (tx, rx) = crossbeam_channel::unbounded::<LogJob>();
    crate::threads::named("run-logs").spawn(move || {
        for job in rx {
            let failed = match job {
                LogJob::Append(log, text) => {
                    use std::io::Write;
                    let written = paths::open_private_append(&log).and_then(|mut f| f.write_all(text.as_bytes()));
                    written.err().map(|e| (log, e.to_string()))
                }
                LogJob::Fold { log, header, output } => fold_output(&log, &header, Some(&output))
                    .err()
                    .map(|e| (log, format!("{e} (the output stays in {})", output.display()))),
                LogJob::Ledger(session, runs) => {
                    save_ledger(&session, &runs).err().map(|e| (ledger_path(&session), format!("{e:#}")))
                }
                LogJob::LedgerSync(session, runs, done) => {
                    done(save_ledger(&session, &runs).map_err(|e| format!("{e:#}")));
                    None
                }
                LogJob::Flush(done) => {
                    let _ = done.send(());
                    None
                }
                LogJob::Then(then) => {
                    then();
                    None
                }
            };
            if let Some((path, e)) = failed {
                warn(format!("run log {}: {e}", paths::shorten_home(&path)));
            }
        }
    })?;
    Ok(tx)
}

/// A run in progress, as the ledger keeps it on disk: after a crash or a kill, the next start
/// says in its log that it was interrupted, with the output its targets had written
/// ([`recover_interrupted`]).
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct LedgerRun {
    pub id: u32,
    pub name: String,
    pub log: PathBuf,
    pub targets: Vec<LedgerTarget>,
}

/// A target of a run in progress: a server, or this machine.
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct LedgerTarget {
    /// What the log calls it.
    pub label: String,
    /// Still to finish (waiting or running).
    pub pending: bool,
    /// The file its output goes to until it finishes and is folded into the log.
    pub output: Option<PathBuf>,
}

fn ledger_path(session: &str) -> PathBuf {
    runs_dir().join(format!(".active-{session}.json"))
}

/// Save the runs `session` has in progress; with none, the ledger goes.
fn save_ledger(session: &str, runs: &[LedgerRun]) -> Result<()> {
    let path = ledger_path(session);
    if runs.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        };
    }
    paths::ensure_private_dir(&runs_dir())?;
    paths::write_private(&path, &serde_json::to_vec(runs)?)?;
    Ok(())
}

/// Close the logs of the runs a previous server of `session` left unfinished; their names.
pub fn recover_interrupted(session: &str) -> Vec<String> {
    let path = ledger_path(session);
    let Ok(text) = std::fs::read(&path) else { return Vec::new() };
    // A ledger that cannot be handled is kept aside for a look (never overwritten), rather than
    // taken for "nothing was running".
    let keep_aside = |why: &str| {
        let aside = path.with_extension(format!("json.{why}-{}", crate::accounts::now()));
        let _ = std::fs::rename(&path, aside);
    };
    let runs: Vec<LedgerRun> = match serde_json::from_slice(&text) {
        Ok(r) => r,
        Err(_) => {
            keep_aside("unreadable");
            return Vec::new();
        }
    };
    let when = local_time(SystemTime::now());
    let mut failed = false;
    for r in &runs {
        for t in r.targets.iter().filter(|t| t.pending) {
            let header = format!("\n--- {} · interrupted\n", t.label);
            let result = match &t.output {
                Some(out) if out.exists() => fold_output(&r.log, &header, Some(out)),
                // Nothing was printed yet (or nothing could be recorded).
                _ => fold_output(&r.log, &header, None),
            };
            failed |= result.is_err();
        }
        let unfinished: Vec<&str> = r.targets.iter().filter(|t| t.pending).map(|t| t.label.as_str()).collect();
        let closing = format!(
            "\n\ninterrupted  {when}  kraal stopped while the run was in progress (unfinished: {})\n",
            unfinished.join(", ")
        );
        failed |= fold_output(&r.log, &closing, None).is_err();
    }
    if failed {
        keep_aside("unfinished");
    } else {
        let _ = std::fs::remove_file(&path);
    }
    runs.into_iter().map(|r| r.name).collect()
}

/// What `kraal runbook new` writes for a TOML runbook.
pub const TEMPLATE_TOML: &str = r#"# A kraal runbook: `kraal runbook run <name>`, or ctrl+b R b.
description = "Check the nginx configuration, reload it, check the service"

# Where it runs: "local" (this machine), "pane" (where the focused pane is) or "server"
# (servers or a @group, chosen when it runs; the focused pane's server is offered first).
on = "server"
# Servers at a time (0: all at once), and whether to stop at the first failure.
rolling = 1
stop_on_failure = true
# shell = "bash"

# Each step is shell code, run with `set -e`: the first command that fails stops the run. Steps
# share the shell, so a `cd` carries on (with stop_on_failure = false, each step runs on its own).
steps = [
  "sudo nginx -t",
  "sudo systemctl reload nginx",
  "systemctl is-active nginx",
]

# Parameters are asked for before the plan is shown, and given to the steps as variables.
# [[param]]
# name = "VERSION"
# prompt = "Version to deploy"
# default = "$KRAAL_BRANCH"
# choices_command = "git tag --sort=-v:refname | head -20"
"#;

/// What `kraal runbook new --markdown` writes.
pub const TEMPLATE_MD: &str = r#"# Restart the API

Run each block in turn: ctrl+b R b, pick this runbook, then enter on a step.

## Check the service

```sh
systemctl status api --no-pager | head -5
```

## Restart it

```sh
sudo systemctl restart api
```

## Follow the logs

```sh
journalctl -fu api
```
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_logs_keep_plain_text() {
        let dir = std::env::temp_dir().join(format!("kraal-outlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = OutputLog::create(dir.join("a.out")).unwrap();
        // Colours, a title (OSC), a progress bar redrawn with \r, CRLF line ends, a split escape.
        for chunk in [
            &b"\x1b[1;32mok\x1b[0m\r\n\x1b]0;title\x07"[..],
            b"10%\r50%\r100%\r\n",
            b"\x1b[3",
            b"1mred\x1b[0m done\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\r\n",
            b"no newline",
        ] {
            log.feed(chunk);
        }
        let (path, error) = log.finish();
        assert!(error.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ok\n100%\nred donelink\nno newline\n");
        let mut big = OutputLog::create(dir.join("b.out")).unwrap();
        let line = vec![b'x'; 1 << 20];
        for _ in 0..20 {
            big.feed(&line);
        }
        let size = std::fs::metadata(big.finish().0).unwrap().len();
        assert!(size > MAX_OUTPUT && size < MAX_OUTPUT + 1024, "{size}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn steps_files() {
        let s = parse_steps(
            "description = \"d\"\non = \"server\"\nsteps = [\"a\", \"b\"]\n[[param]]\nname = \"VERSION\"\ndefault = \"$KRAAL_BRANCH\"\n",
        )
        .unwrap();
        assert_eq!((s.on, s.rolling, s.stop_on_failure, s.params.len()), (On::Server, 1, true, 1));
        assert!(parse_steps("steps = []").is_err());
        assert!(parse_steps("steps = [\"a\"]\n[[param]]\nname = \"bad name\"\n").is_err());
        assert!(parse_steps("steps = [\"a\"]\n[[param]]\nname = \"KRAAL_X\"\n").is_err());
        assert!(parse_steps("steps = [\"a\"]\ntypo = 1\n").is_err());
        // Read past, this misspelt key would show a secret in the plan and the log.
        assert!(parse_steps("steps = [\"a\"]\n[[param]]\nname = \"TOKEN\"\nsecrte = true\n").is_err());
        assert!(parse_steps("steps = [\"a\"]\nshell = \"bash;rm\"\n").is_err());
        assert!(parse_steps("steps = [\"a\"]\nshell = \"/usr/bin/bash\"\n").is_ok());
        assert!(parse_steps(TEMPLATE_TOML).is_ok());
        assert!(parse_steps("steps = [\"a\"]\ntimeout = \"10 minutes\"\n").is_err());
        let ten = parse_steps("steps = [\"a\"]\ntimeout = \"10m\"\n").unwrap().timeout();
        assert_eq!(ten, Some(std::time::Duration::from_secs(600)));
        assert_eq!(parse_duration("90"), Some(std::time::Duration::from_secs(90)));
        assert_eq!(parse_duration("0s"), None);
        let vars = vec![("KRAAL_BRANCH".to_string(), "main".to_string()), ("KRAAL_B".to_string(), "x".to_string())];
        assert_eq!(expand_vars("v-$KRAAL_BRANCH-${KRAAL_B}", &vars), "v-main-x");
    }

    #[test]
    fn a_parameter_takes_one_of_its_choices() {
        let params = "[[param]]\nname = \"SITE\"\n[[param]]\nname = \"TOKEN\"\nsecret = true\n";
        let steps = parse_steps(&format!("steps = [\"a\"]\n{params}")).unwrap();
        let (site, token) = (&steps.params[0], &steps.params[1]);
        let choices = ["shop".to_string(), "blog".to_string()];
        assert!(site.accept("blog", &choices).is_ok());
        let refused = site.accept("Blog", &choices).unwrap_err().to_string();
        assert_eq!(refused, "SITE=Blog is not one of its choices: shop, blog");
        assert!(site.accept("", &choices).is_err());
        // Without choices, the question is a field: anything goes, nothing included.
        assert!(site.accept("anything", &[]).is_ok() && site.accept("", &[]).is_ok());
        // A secret is not repeated, refused or not.
        let refused = token.accept("hunter2", &choices).unwrap_err().to_string();
        assert_eq!(refused, "TOKEN=•••• is not one of its choices: shop, blog");
    }

    #[test]
    fn a_runbook_directory_cannot_hang_the_listing() {
        let dir = std::env::temp_dir().join(format!("kraal-rbdir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.toml"), "steps = [\"true\"]\n").unwrap();
        let _ = std::fs::remove_file(dir.join("hang.toml"));
        std::os::unix::fs::symlink("/dev/zero", dir.join("hang.toml")).unwrap();
        let loaded = load_dir(&dir, &Source::User);
        assert_eq!(loaded.len(), 2);
        assert!(loaded[0].is_err(), "hang.toml is refused");
        assert_eq!(loaded[1].as_ref().unwrap().name, "ok");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn markdown_blocks() {
        let (title, blocks) = parse_markdown(TEMPLATE_MD);
        assert_eq!(title, "Restart the API");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[1], Block { heading: "Restart it".into(), code: "sudo systemctl restart api".into() });
        let (_, b) =
            parse_markdown("# T\n```console\n$ uptime\n 10:00 up 3 days\n$ df -h\n```\n```python\nprint(1)\n```\n");
        assert_eq!(b, vec![Block { heading: "T".into(), code: "uptime\ndf -h".into() }]);
        let (_, b) = parse_markdown("```console\n$ docker run \\\n>   -it alpine\nhello\n```\n");
        assert_eq!(b[0].code, "docker run \\\n  -it alpine");
    }

    #[test]
    fn scripts_run_and_stop() {
        let run = |script: &str| {
            let out = std::process::Command::new("sh").arg("-c").arg(script).output().unwrap();
            (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let steps = vec!["echo one".to_string(), "false\necho after-false".to_string(), "echo three".to_string()];
        let (code, out) = run(&script(&steps, true));
        assert_eq!(code, Some(1));
        assert!(out.contains("one") && out.contains("step 2 failed (exit 1)"), "{out}");
        assert!(!out.contains("after-false") && !out.contains("three"), "a failing line stops the step: {out}");
        let (code, out) = run(&script(&steps, false));
        assert_eq!(code, Some(1));
        assert!(!out.contains("after-false"), "{out}");
        assert!(out.contains("three") && out.contains("some steps failed"), "{out}");
        // Pipelines fail when a part fails, where the shell knows pipefail.
        let (code, _) = run(&script(&["false | cat".to_string()], true));
        let pipefail = std::process::Command::new("sh").arg("-c").arg("set -o pipefail").status().unwrap().success();
        assert_eq!(code == Some(0), !pipefail);
        let (code, out) =
            run(&script(&["cd /tmp".to_string(), "X=kept".to_string(), "pwd; echo $X".to_string()], true));
        assert_eq!(code, Some(0));
        assert!(out.contains("/tmp\nkept") && out.contains("✓ done"), "steps share the shell: {out}");
        let (code, out) = run(&command("bash", &script(&["echo \"it's $BASH_VERSION\"".to_string()], true)));
        assert_eq!(code, Some(0));
        assert!(out.contains("it's ") && !out.contains("it's \n"), "runs in bash: {out}");
    }

    #[test]
    fn repo_runbooks_need_trust() {
        let dir = std::env::temp_dir().join(format!("kraal-rb-{}", std::process::id()));
        let rdir = repo_dir(&dir);
        std::fs::create_dir_all(&rdir).unwrap();
        std::fs::write(rdir.join("deploy.toml"), "steps = [\"echo hi\"]\n").unwrap();
        std::fs::write(rdir.join("notes.txt"), "ignored").unwrap();
        let list = load_dir(&rdir, &Source::Repo(dir.clone()));
        assert_eq!(list.len(), 1);
        let r = list.into_iter().next().unwrap().unwrap();
        assert_eq!(r.name, "deploy");
        assert!(matches!(r.source, Source::Repo(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_repository_runbook_is_shown_with_nothing_hidden() {
        // An escape sequence that erases the line, a carriage return that writes over it, and
        // text a terminal would show in another order.
        let content = "# Notes\r\n```sh\nrm -rf ~ \x1b[2K\rls\n\u{202e}dne\n```\n";
        assert_eq!(Source::User.shown(content), content, "the user's own file is shown as it is");
        let shown = Source::Repo(PathBuf::from("/repo")).shown(content);
        assert_eq!(shown, "# Notes\n```sh\nrm -rf ~ \\u{1b}[2K\\rls\n\\u{202e}dne\n```\n");
    }

    #[test]
    fn a_repository_runbook_is_known_by_its_real_place() {
        let dir = std::env::temp_dir().join(format!("kraal-rb-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for repo in ["real/app", "other/app"] {
            std::fs::create_dir_all(repo_dir(&dir.join(repo))).unwrap();
            std::fs::write(repo_dir(&dir.join(repo)).join("deploy.toml"), "steps = [\"true\"]\n").unwrap();
        }
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let (real, other) =
            (dir.join("real/app").canonicalize().unwrap(), dir.join("other/app").canonicalize().unwrap());
        let spelled = dir.join("link/app");
        // Read through the link: the repository and the file are where they really are.
        let through = repo_dir(&spelled).join("deploy.toml");
        let r = load(&through, source_of(&through)).unwrap();
        assert_eq!(
            (r.source.clone(), r.path.clone()),
            (Source::Repo(real.clone()), repo_dir(&real).join("deploy.toml"))
        );
        let file = file_in_repo(&r, &real);
        assert_eq!(file, Path::new(".kraal/runbooks/deploy.toml"));
        assert_eq!(approval_key(&real, file), r.path.display().to_string());
        // A key written through the link names this file; no other key does.
        assert!(other_spelling(&approval_key(&spelled, file), &real, file));
        let others = [
            approval_key(&real, file),
            approval_key(&other, file),
            approval_key(&spelled, Path::new(".kraal/runbooks/other.toml")),
            approval_key(&spelled, Path::new(".kraal/worktree.toml")),
            approval_key(&dir.join("gone/app"), file),
        ];
        for key in others {
            assert!(!other_spelling(&key, &real, file), "{key}");
        }
        // The link comes to lead elsewhere: what was written through it names this file no more.
        std::fs::remove_file(dir.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.join("other"), dir.join("link")).unwrap();
        assert!(!other_spelling(&approval_key(&spelled, file), &real, file));
        let _ = std::fs::remove_dir_all(dir);
    }
}
