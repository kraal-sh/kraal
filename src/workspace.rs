//! Workspace definitions loaded from KDL files.
//!
//! A workspace file is a Zellij layout, so existing `~/.config/zellij/layouts/*.kdl`
//! files work unchanged. Zellij writes KDL v1; KDL v2 is accepted too. Only the
//! subset kraal needs is interpreted: anything else is skipped and reported in
//! [`WorkspaceDef::warnings`] rather than failing the load.
//!
//! kraal extends panes with two properties: `preview="notes.md"` (show a rendered
//! file instead of a terminal) and `agent="claude"` (the pane runs a coding agent).
//! Zellij rejects unknown properties, so files using them are kraal-only. Stacks are
//! Zellij's: `pane stacked=true { ... }`, with `expanded=true` on the pane open in it.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::{env, fs, io};

use anyhow::{Context as _, Result, anyhow};
use kdl::{KdlDocument, KdlEntry, KdlError, KdlNode, KdlValue};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Empty,
    Control,
    Used,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "Type a name.",
            Self::Control => "Choose a name without control characters.",
            Self::Used => "This name is already used. Choose another name.",
        })
    }
}

impl std::error::Error for NameError {}

pub fn validate_name(name: &str, used: bool) -> Result<(), NameError> {
    if name.trim().is_empty() {
        Err(NameError::Empty)
    } else if name.chars().any(char::is_control) {
        Err(NameError::Control)
    } else if used {
        Err(NameError::Used)
    } else {
        Ok(())
    }
}

/// How a split arranges its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Axis {
    /// Children placed left to right (Zellij `split_direction="vertical"`).
    Row,
    /// Children placed top to bottom (Zellij `split_direction="horizontal"`, the default).
    #[default]
    Column,
}

impl Axis {
    fn from_zellij(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("vertical") {
            Some(Self::Row)
        } else if s.eq_ignore_ascii_case("horizontal") {
            Some(Self::Column)
        } else {
            None
        }
    }

    fn zellij_name(self) -> &'static str {
        match self {
            Self::Row => "vertical",
            Self::Column => "horizontal",
        }
    }
}

/// Size of a split child along its parent's axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SizeSpec {
    /// A fixed number of cells (columns in a row, lines in a column).
    Fixed(u16),
    /// A percentage (1-100) of the parent's extent.
    Percent(u16),
}

/// What a pane shows.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PaneKind {
    /// A terminal running [`PaneDef::command`], or the default shell.
    #[default]
    Terminal,
    /// A rendered preview of a file (kraal extension `preview="..."`).
    Preview(PathBuf),
}

/// A leaf pane.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PaneDef {
    /// Display name (Zellij `name`).
    pub name: Option<String>,
    /// Program to run; `None` runs the default shell.
    pub command: Option<String>,
    /// Arguments for `command`.
    pub args: Vec<String>,
    /// Effective working directory, already resolved against the enclosing tab and workspace.
    pub cwd: Option<PathBuf>,
    /// Whether this pane should get the focus in its tab.
    pub focus: bool,
    /// Terminal or file preview.
    pub kind: PaneKind,
    /// Coding agent hint (kraal extension `agent="..."`).
    pub agent: Option<String>,
    /// Saved identity of an explicitly chosen account; `agent` keeps its readable label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Wait for a keypress before starting `command`.
    pub start_suspended: bool,
    /// Close the pane when `command` exits instead of holding it open.
    pub close_on_exit: bool,
}

/// A tiled layout tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutNode {
    /// A single pane.
    Pane(PaneDef),
    /// Two or more children laid out along `axis`, each with an optional size.
    Split {
        /// Direction in which the children are placed.
        axis: Axis,
        /// Children in order, with their size along `axis` (`None` shares the remaining space).
        children: Vec<(LayoutNode, Option<SizeSpec>)>,
    },
    /// Two or more panes sharing one place (Zellij `stacked=true`): one shown whole, the others
    /// as a title line.
    Stack {
        panes: Vec<PaneDef>,
        /// Index of the pane shown whole (Zellij `expanded=true`).
        expanded: usize,
    },
}

impl LayoutNode {
    /// Leaf panes in depth-first order.
    pub fn panes(&self) -> Vec<&PaneDef> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes<'a>(&'a self, out: &mut Vec<&'a PaneDef>) {
        match self {
            Self::Pane(pane) => out.push(pane),
            Self::Split { children, .. } => {
                for (child, _) in children {
                    child.collect_panes(out);
                }
            }
            Self::Stack { panes, .. } => out.extend(panes),
        }
    }

    fn into_panes(self) -> Vec<PaneDef> {
        match self {
            Self::Pane(pane) => vec![pane],
            Self::Split { children, .. } => children.into_iter().flat_map(|(child, _)| child.into_panes()).collect(),
            Self::Stack { panes, .. } => panes,
        }
    }
}

/// A tab: a tiled layout plus floating panes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabDef {
    /// Tab title (Zellij `name`).
    pub name: Option<String>,
    /// Whether this tab should be active when the workspace opens.
    pub focus: bool,
    /// Effective working directory, already resolved against the workspace cwd.
    pub cwd: Option<PathBuf>,
    /// Tiled panes.
    pub root: LayoutNode,
    /// Floating panes, in declaration order.
    pub floating: Vec<PaneDef>,
}

impl TabDef {
    /// Tiled panes in depth-first order, then floating panes.
    pub fn panes(&self) -> Vec<&PaneDef> {
        let mut out = self.root.panes();
        out.extend(&self.floating);
        out
    }
}

/// A parsed workspace file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDef {
    /// Display name, independent of the source file name.
    pub name: String,
    /// Workspace-wide working directory (Zellij `cwd "..."` inside `layout`).
    pub cwd: Option<PathBuf>,
    /// Tabs in order; never empty for a parsed file.
    pub tabs: Vec<TabDef>,
    /// Constructs that were skipped, as `line N: message`.
    pub warnings: Vec<String>,
}

impl WorkspaceDef {
    /// Every pane in tab order (tiled then floating), handy for listings.
    pub fn panes(&self) -> Vec<&PaneDef> {
        self.tabs.iter().flat_map(TabDef::panes).collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportPreview {
    pub path: PathBuf,
    pub revision: crate::paths::FileRevision,
    pub definition: WorkspaceDef,
    pub accounts: Vec<ImportAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportAccount {
    pub reference: String,
    pub agent: String,
    pub choices: Vec<(String, String)>,
}

impl ImportPreview {
    pub fn read(path: &Path) -> Result<Self> {
        let path = definition_path(path);
        let revision = crate::paths::FileRevision::read(&path)?;
        let source =
            std::str::from_utf8(revision.bytes.as_deref().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "File missing. Choose a KDL file.")
            })?)?;
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        let definition = parse_workspace(&name, source).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Self { path, revision, definition, accounts: Vec::new() })
    }

    pub fn summary(&self) -> String {
        let mut text = format!(
            "{} tab{} · {} pane{}\nCommands wait for you to start them.",
            self.definition.tabs.len(),
            if self.definition.tabs.len() == 1 { "" } else { "s" },
            self.definition.panes().len(),
            if self.definition.panes().len() == 1 { "" } else { "s" }
        );
        for warning in &self.definition.warnings {
            write!(text, "\n{warning}").unwrap();
        }
        text
    }
}

impl WorkspaceDef {
    pub fn visit_panes(&mut self, mut visit: impl FnMut(&mut PaneDef)) {
        fn node(n: &mut LayoutNode, visit: &mut impl FnMut(&mut PaneDef)) {
            match n {
                LayoutNode::Pane(p) => visit(p),
                LayoutNode::Stack { panes, .. } => panes.iter_mut().for_each(visit),
                LayoutNode::Split { children, .. } => {
                    for (n, _) in children {
                        node(n, visit);
                    }
                }
            }
        }
        for tab in &mut self.tabs {
            node(&mut tab.root, &mut visit);
            tab.floating.iter_mut().for_each(&mut visit);
        }
    }

    pub fn portable_accounts(&mut self) {
        self.visit_panes(|pane| {
            if let Some(agent) = &mut pane.agent
                && crate::accounts::agent_kind(agent.split(':').next().unwrap()).is_some()
            {
                *agent = agent.split(':').next().unwrap().to_string();
                pane.account_id = None;
            }
        });
    }
}

pub fn from_snapshot(ws: &crate::persist::WorkspaceSnap, name: &str) -> WorkspaceDef {
    use crate::persist::{NodeSnap, PaneSnap};
    fn pane(p: &PaneSnap, focused: bool) -> PaneDef {
        let spec = &p.spec;
        let mut def = PaneDef {
            name: p.name.clone(),
            cwd: spec.cwd.clone(),
            focus: focused,
            close_on_exit: spec.close_on_exit,
            start_suspended: p.stopped,
            ..Default::default()
        };
        if let Some(path) = &spec.preview {
            def.kind = PaneKind::Preview(path.clone());
        } else if let Some(pending) = &spec.pending_agent {
            def.agent = Some(pending.agent.clone());
            def.args = pending.args.clone();
            def.start_suspended = pending.suspended;
        } else if let Some(account) = &spec.account {
            def.agent = Some(account.clone());
            def.account_id = spec.account_id.clone();
            let kind = account.split_once(':').and_then(|(k, _)| crate::accounts::agent_kind(k));
            def.args = kind.map(|k| crate::accounts::launch_args(k, &spec.command)).unwrap_or_default();
            def.start_suspended |=
                !kind.and_then(|k| crate::accounts::resumable_args(k, &def.args)).is_some_and(|(_, exact)| exact);
        } else {
            def.command = spec.command.first().cloned();
            def.args = spec.command.iter().skip(1).cloned().collect();
            if spec.command.is_empty() {
                def.agent = p.agent.clone();
            }
        }
        def
    }
    fn node(n: &NodeSnap, focus: Option<usize>, index: &mut usize) -> LayoutNode {
        match n {
            NodeSnap::Pane(p) => {
                let def = pane(p, focus == Some(*index));
                *index += 1;
                LayoutNode::Pane(def)
            }
            NodeSnap::Split { axis, children, stacked, open } => {
                let sum = children.iter().map(|(_, w)| w).sum::<f32>().max(f32::EPSILON);
                let children: Vec<_> = children
                    .iter()
                    .map(|(n, w)| {
                        (node(n, focus, index), Some(SizeSpec::Percent(((w / sum) * 100.0).round().max(1.0) as u16)))
                    })
                    .collect();
                if *stacked {
                    LayoutNode::Stack {
                        panes: children.iter().flat_map(|(n, _)| n.panes().into_iter().cloned()).collect(),
                        expanded: open.unwrap_or(0),
                    }
                } else {
                    LayoutNode::Split { axis: *axis, children }
                }
            }
        }
    }
    let tabs = ws
        .tabs
        .iter()
        .enumerate()
        .map(|(ti, tab)| {
            let mut index = 0;
            let root = tab
                .root
                .as_ref()
                .map(|n| node(n, tab.focus, &mut index))
                .unwrap_or_else(|| LayoutNode::Pane(PaneDef { cwd: ws.cwd.clone(), ..Default::default() }));
            let floating = tab
                .floating
                .iter()
                .filter(|(p, _)| p.spec.login.is_none())
                .map(|(p, _)| {
                    let def = pane(p, tab.focus == Some(index));
                    index += 1;
                    def
                })
                .collect();
            TabDef {
                name: (!tab.name.is_empty()).then(|| tab.name.clone()),
                focus: ti == ws.active,
                cwd: None,
                root,
                floating,
            }
        })
        .collect();
    WorkspaceDef { name: name.into(), cwd: ws.cwd.clone(), tabs, warnings: Vec::new() }
}

/// Parse a workspace file. `name` is the workspace name (file stem).
///
/// Fails only on invalid KDL or a missing `layout` node. Relative `cwd` values are
/// resolved against the enclosing cwd (pane, then tab, then workspace) and a
/// leading `~/` is expanded with `$HOME`; panes and tabs without a `cwd` inherit it.
pub fn parse_workspace(name: &str, src: &str) -> Result<WorkspaceDef> {
    let doc = parse_kdl(src)?;
    let mut parser = Parser { src, warnings: Vec::new() };
    let mut layout = None;
    for node in doc.nodes() {
        match node.name().value() {
            "layout" if layout.is_none() => layout = Some(node),
            "layout" => parser.warn(node_pos(node), "extra `layout` node ignored"),
            other => parser.warn(node_pos(node), format!("unknown top-level node `{other}` ignored")),
        }
    }
    let layout = layout.context("no `layout` node")?;
    let (cwd, tabs) = parser.layout(layout);
    // Report in document order (the workspace cwd is read ahead of the rest).
    parser.warnings.sort_by_key(|(offset, _)| *offset);
    Ok(WorkspaceDef {
        name: name.to_owned(),
        cwd,
        tabs,
        warnings: parser.warnings.into_iter().map(|(_, msg)| msg).collect(),
    })
}

/// Whether `name` can name a definition file (`<name>.kdl`) inside the definitions directory:
/// one plain path component, so that it never reaches another directory, and no control
/// character (a newline would split the list of saved workspaces).
pub fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control)
        && Path::new(name).components().count() == 1
}

/// The file name a workspace is saved under when none is given: its name when that is already
/// one plain name ([`valid_file_name`]). A group running a command is named after the whole
/// command, which may hold a `/` or be long: those become `_`, a name past 200 bytes is cut, and
/// whenever the name had to change, a digest of the whole is added, so that two workspaces never
/// share a file (`tail /var/log/a_b` and `tail /var/log/a/b` would otherwise both be
/// `tail _var_log_a_b`).
pub fn file_name_from(name: &str) -> String {
    if valid_file_name(name) && name.trim() == name {
        return name.to_string();
    }
    let replaced: String =
        name.chars().map(|c| if matches!(c, '/' | '\\' | '\0') || c.is_control() { '_' } else { c }).collect();
    // Trimmed first: a dot the spaces hid must not start the file name.
    let trimmed = replaced.trim();
    let out = match trimmed.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None if trimmed.is_empty() => "workspace".to_string(),
        None => trimmed.to_string(),
    };
    let digest = format!("-{:016x}", crate::text::fnv64(name));
    let mut cut = out.len().min(200 - digest.len());
    while !out.is_char_boundary(cut) {
        cut -= 1;
    }
    let file = format!("{}{digest}", &out[..cut]);
    if valid_file_name(&file) { file } else { format!("workspace{digest}") }
}

/// Load every `*.kdl` in `dir`, sorted by name, each listed under its [`definition_path`].
/// Unparseable files are returned as `Err` entries with the path.
///
/// A missing directory yields an empty list; any other error reading it yields a
/// single `Err` entry for `dir` itself.
pub fn load_dir(dir: &Path) -> Vec<(PathBuf, Result<WorkspaceDef>)> {
    let dir = &fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            let err = anyhow::Error::new(e).context(format!("reading {}", dir.display()));
            return vec![(dir.to_path_buf(), Err(err))];
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "kdl") && path.is_file())
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let ws = load_file(&path);
            (path, ws)
        })
        .collect()
}

/// The path a definition file is known by: its directory resolved (links, `..`), so that a
/// workspace opened through another spelling of that directory is still linked to its file. The
/// file itself is not resolved: a definition that is a link is renamed as a link.
pub fn definition_path(path: &Path) -> PathBuf {
    match (path.parent().and_then(|dir| fs::canonicalize(dir).ok()), path.file_name()) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    }
}

/// The definition in the file at `path`, named after the file.
pub fn load_file(path: &Path) -> Result<WorkspaceDef> {
    let src = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let name = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    parse_workspace(&name, &src).with_context(|| format!("parsing {}", path.display()))
}

/// Bind a retained recipe without writing its external file. Insert at source spans:
/// serializing a v1 document through the v2 parser can change comments and booleans.
pub fn bind_source_accounts(source: &str, accounts: &crate::accounts::Accounts) -> Result<String> {
    fn bindings(doc: &KdlDocument, accounts: &crate::accounts::Accounts) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for node in doc.nodes() {
            if node.name().value() == "pane"
                && node.get("account_id").is_none()
                && let Some(entry) =
                    node.entries().iter().rev().find(|e| e.name().is_some_and(|n| n.value() == "agent"))
                && let Some((agent, name)) = entry.value().as_string().and_then(|s| s.split_once(':'))
                && let Some(a) = accounts.get(agent, name)
            {
                out.push((entry.span().offset() + entry.span().len(), format!(" account_id={}", quote(&a.id))));
            }
            if let Some(children) = node.children() {
                out.extend(bindings(children, accounts));
            }
        }
        out
    }
    let mut insertions = bindings(&parse_kdl(source)?, accounts);
    let mut source = source.to_string();
    if !insertions.is_empty() {
        insertions.sort_by_key(|(offset, _)| *offset);
        for (offset, text) in insertions.into_iter().rev() {
            source.insert_str(offset, &text);
        }
        parse_kdl(&source).context("checking workspace account identities")?;
    }
    Ok(source)
}

/// Save the revision that was loaded or confirmed, keeping the replaced file beside it.
pub fn save_file(path: &Path, def: &WorkspaceDef, expected: &crate::paths::FileRevision) -> Result<String> {
    let source = to_kdl(def);
    crate::paths::write_checked(path, source.as_bytes(), expected)
        .with_context(|| format!("saving {}", path.display()))?;
    Ok(source)
}

/// Ownership must stay stable through publication: called by the server, or under the
/// stopped session's lock. A file revision alone cannot detect a workspace being opened.
pub fn publish_import<'a>(
    path: &Path,
    source: &str,
    expected: &crate::paths::FileRevision,
    sources: impl Iterator<Item = (&'a str, &'a Path)>,
) -> Result<()> {
    for (name, owned) in sources {
        anyhow::ensure!(
            !(crate::paths::same_file(owned, path)
                || expected.bytes.is_some()
                    && crate::paths::same_file(owned, &expected.target.with_extension("kdl.bak"))),
            "destination belongs to workspace {name:?}; choose another file name"
        );
    }
    parse_workspace(&path.file_stem().unwrap_or_default().to_string_lossy(), source)?;
    crate::paths::write_checked(path, source.as_bytes(), expected)?;
    Ok(())
}

/// What to say when no workspace is named `name`: none is open under that name, and no file of
/// `dirs` defines one. When the name reads as a server's (`names_server`: someone typed `kraal
/// attach fpa@anvil`), it says how to attach to the kraal there, which is what was meant.
pub fn unknown(name: &str, dirs: &[PathBuf], names_server: bool) -> String {
    let dirs: Vec<String> = dirs.iter().map(|dir| crate::paths::shorten_home(dir)).collect();
    let plain = format!("no workspace named {name:?} in {}", dirs.join(", "));
    if names_server { format!("{plain}; to attach to kraal on that server: kraal attach --host {name}") } else { plain }
}

/// Serialise back to KDL (v1 syntax, Zellij-compatible, kraal extensions as
/// properties), so that `parse_workspace(name, &to_kdl(&ws))` round-trips.
///
/// The output includes Zellij's usual tab-bar/status-bar `default_tab_template`
/// (ignored by kraal) so the file also looks right when opened in Zellij. Paths are
/// written relative to the enclosing cwd only when they are relative themselves.
pub fn to_kdl(ws: &WorkspaceDef) -> String {
    let mut w = Writer::default();
    w.line(0, "layout {");
    if let Some(cwd) = &ws.cwd {
        w.line(1, &format!("cwd {}", quote(&path_text(cwd, None))));
    }
    for line in DEFAULT_TAB_TEMPLATE.lines() {
        w.line(1, line);
    }
    for tab in &ws.tabs {
        let mut head = String::from("tab");
        push_str(&mut head, "name", tab.name.as_deref());
        push_flag(&mut head, "focus", tab.focus);
        push_cwd(&mut head, tab.cwd.as_deref(), ws.cwd.as_deref());
        head.push_str(" {");
        w.line(1, &head);
        let base = tab.cwd.as_deref().or(ws.cwd.as_deref());
        w.node(&tab.root, None, base, 2);
        if !tab.floating.is_empty() {
            w.line(2, "floating_panes {");
            for pane in &tab.floating {
                w.pane(pane, None, base, 3, false);
            }
            w.line(2, "}");
        }
        w.line(1, "}");
    }
    w.line(0, "}");
    w.out
}

// ---------------------------------------------------------------------------
// Parsing

/// A layout node with its size in the parent split.
type SizedNode = (LayoutNode, Option<SizeSpec>);

/// Blocks nested deeper than any layout nests them: both parsers read a block within a block
/// by calling themselves, and a file nested a couple of thousand deep overflows the stack (some
/// sixty in a debug build, on a thread of the default size).
const MAX_NESTING: usize = 32;

/// Blocks commented out (`/-`) within blocks commented out, deeper than any layout has them:
/// both parsers read such a block again for each one around it, in a time that doubles with
/// each (a kilobyte nesting twenty takes seconds, a few kilobytes hours).
const MAX_COMMENTED_NESTING: usize = 6;

/// Parses the KDL text, trying v2 first and then v1 (Zellij's dialect).
fn parse_kdl(src: &str) -> Result<KdlDocument> {
    // The parsers read strings their own way, and v2 goes on past an error: braces count as
    // each dialect reads them, and as plain bytes.
    for reading in [Reading::Bytes, Reading::V1, Reading::V2] {
        check_nesting(src.as_bytes(), reading)?;
    }
    // A parser that panics (kdl 4.7's v1 does on `(true` ending a line) does not read the file.
    let v2_err = match std::panic::catch_unwind(|| KdlDocument::parse_v2(src)) {
        Ok(Ok(doc)) => return Ok(doc),
        Ok(Err(e)) => Some(e),
        Err(_) => None,
    };
    let v1_err = match std::panic::catch_unwind(|| KdlDocument::parse_v1(src)) {
        Ok(Ok(doc)) => return Ok(doc),
        Ok(Err(e)) => Some(e),
        Err(_) => None,
    };
    // Neither dialect fits: report the parser that got further, which most likely
    // matches the dialect the author intended. Ties go to v1, Zellij's dialect.
    let err = match (&v2_err, &v1_err) {
        (Some(v2), Some(v1)) => {
            if error_offset(v2) > error_offset(v1) {
                v2
            } else {
                v1
            }
        }
        (Some(e), None) | (None, Some(e)) => e,
        (None, None) => anyhow::bail!("invalid KDL"),
    };
    Err(anyhow!("invalid KDL: {}", describe_error(src, err)))
}

/// How [`check_nesting`] reads a KDL text.
#[derive(Clone, Copy, PartialEq)]
enum Reading {
    /// Every byte is what it is: no strings, no comments.
    Bytes,
    /// Strings go on over lines; `"""` is an empty string and another.
    V1,
    /// A line ends a string, unless `"""` and the line's end opened it.
    V2,
}

/// Refuses blocks nested deeper than the parsers take ([`MAX_NESTING`],
/// [`MAX_COMMENTED_NESTING`]), reading strings and comments as `reading` says. A `/-` counts for
/// the block that follows in its node, even when it comments out only an argument before it.
fn check_nesting(src: &[u8], reading: Reading) -> Result<()> {
    // Whether each open block is commented out, and how many are.
    let mut open: Vec<bool> = Vec::new();
    let mut commented = 0;
    // A `/-` waiting for what it comments out (on a line below, maybe), or in its node.
    let (mut slashdash, mut target) = (false, false);
    // A `\` ending the line: the node goes on.
    let mut continued = false;
    let words = reading != Reading::Bytes;
    let mut i = 0;
    while i < src.len() {
        let b = src[i];
        let next = src.get(i + 1).copied();
        i += 1;
        match b {
            b'"' if words => i = string_end(src, i, None, reading),
            b'#' | b'r' if words => {
                if let Some(hashes) = raw_hashes(src, i - 1) {
                    i = string_end(src, i + usize::from(b == b'r') + hashes, Some(hashes), reading);
                }
            }
            b'/' if words && next == Some(b'/') => {
                i = src[i..].iter().position(|&c| c == b'\n').map_or(src.len(), |n| i + n);
                continue;
            }
            b'/' if words && next == Some(b'*') => {
                let mut depth = 1;
                i += 1;
                while i < src.len() && depth > 0 {
                    match &src[i..src.len().min(i + 2)] {
                        b"/*" => (depth, i) = (depth + 1, i + 2),
                        b"*/" => (depth, i) = (depth - 1, i + 2),
                        _ => i += 1,
                    }
                }
                continue;
            }
            b'/' if next == Some(b'-') => {
                (slashdash, target) = (true, false);
                i += 1;
                continue;
            }
            b'{' => {
                open.push(slashdash);
                commented += usize::from(slashdash);
                anyhow::ensure!(open.len() <= MAX_NESTING, "invalid KDL: blocks nested more than {MAX_NESTING} deep");
                anyhow::ensure!(
                    commented <= MAX_COMMENTED_NESTING,
                    "invalid KDL: blocks commented out (/-) nested more than {MAX_COMMENTED_NESTING} deep"
                );
                slashdash = false;
            }
            b'}' => {
                commented -= open.pop().map_or(0, usize::from);
                slashdash = false;
            }
            b';' if target => slashdash = false,
            b'\n' if target && !continued => slashdash = false,
            b'\\' => continued = true,
            _ => {}
        }
        if !b.is_ascii_whitespace() {
            target |= slashdash;
            continued &= b == b'\\';
        }
    }
    Ok(())
}

/// The hashes of the raw string starting at `at` (`r"`, `r#"`, `#"`, `##"`...), if one does.
fn raw_hashes(src: &[u8], at: usize) -> Option<usize> {
    let r = src[at] == b'r';
    // An `r` ending a name starts no string.
    if r && at > 0 && !b" \t\r\n=({};".contains(&src[at - 1]) {
        return None;
    }
    let after = at + usize::from(r);
    let hashes = src[after..].iter().take_while(|&&c| c == b'#').count();
    (src.get(after + hashes) == Some(&b'"') && (r || hashes > 0)).then_some(hashes)
}

/// Where the string ends whose text starts at `from`, past its opening quote, as `reading` reads
/// it: past its closing quote, or at the end of its line (v2) or of the text. `raw` gives the
/// hashes of a raw string, which escapes nothing.
fn string_end(src: &[u8], from: usize, raw: Option<usize>, reading: Reading) -> usize {
    let rest = &src[from..];
    let multiline = reading == Reading::V2
        && rest.starts_with(b"\"\"")
        && rest[2..].iter().find(|c| !b" \t\r".contains(c)).is_none_or(|&c| c == b'\n');
    let quotes = if multiline { 3 } else { 1 };
    let mut end = vec![b'"'; quotes];
    end.resize(quotes + raw.unwrap_or(0), b'#');
    let mut i = from + quotes - 1;
    while i < src.len() {
        if raw.is_none() && src[i] == b'\\' {
            i += 2;
        } else if src[i..].starts_with(&end) {
            return i + end.len();
        } else if src[i] == b'\n' && reading == Reading::V2 && !multiline {
            return i;
        } else {
            i += 1;
        }
    }
    src.len()
}

fn error_offset(err: &KdlError) -> usize {
    err.diagnostics.iter().map(|d| d.span.offset()).min().unwrap_or(0)
}

fn describe_error(src: &str, err: &KdlError) -> String {
    let parts: Vec<String> = err
        .diagnostics
        .iter()
        .take(3)
        .map(|d| {
            let line = line_of(src, d.span.offset());
            let msg = d.message.as_deref().unwrap_or("syntax error");
            match &d.help {
                Some(help) => format!("line {line}: {msg} ({help})"),
                None => format!("line {line}: {msg}"),
            }
        })
        .collect();
    if parts.is_empty() { "syntax error".to_owned() } else { parts.join("; ") }
}

/// 1-based line number of a byte offset.
fn line_of(src: &str, offset: usize) -> usize {
    let end = offset.min(src.len());
    src.as_bytes()[..end].iter().filter(|&&b| b == b'\n').count() + 1
}

fn node_pos(node: &KdlNode) -> usize {
    node.name().span().offset()
}

fn entry_pos(entry: &KdlEntry) -> usize {
    entry.span().offset()
}

fn entry_key(entry: &KdlEntry) -> Option<&str> {
    entry.name().map(|name| name.value())
}

fn child_nodes(node: &KdlNode) -> &[KdlNode] {
    node.children().map_or(&[], KdlDocument::nodes)
}

/// Converts a scalar to text; numbers and booleans are accepted where Zellij wants strings.
fn scalar_text(value: &KdlValue) -> Option<String> {
    match value {
        KdlValue::String(s) => Some(s.clone()),
        KdlValue::Integer(n) => Some(n.to_string()),
        KdlValue::Float(f) => Some(f.to_string()),
        KdlValue::Bool(b) => Some(b.to_string()),
        KdlValue::Null => None,
    }
}

/// Renders a value for messages; strings are always quoted.
fn show(value: &KdlValue) -> String {
    match value {
        KdlValue::String(s) => format!("{s:?}"),
        KdlValue::Integer(n) => n.to_string(),
        KdlValue::Float(f) => f.to_string(),
        KdlValue::Bool(b) => b.to_string(),
        KdlValue::Null => "null".to_owned(),
    }
}

/// Whether `node` has the property `key` set to true.
fn is_set(node: &KdlNode, key: &str) -> bool {
    node.entries().iter().any(|e| entry_key(e) == Some(key) && scalar_text(e.value()).as_deref() == Some("true"))
}

fn parse_size(value: &KdlValue) -> Option<SizeSpec> {
    match value {
        KdlValue::Integer(n) => u16::try_from(*n).ok().filter(|&n| n > 0).map(SizeSpec::Fixed),
        KdlValue::String(s) => {
            let s = s.trim();
            match s.strip_suffix('%') {
                Some(pct) => {
                    pct.trim().parse::<u16>().ok().filter(|pct| (1..=100).contains(pct)).map(SizeSpec::Percent)
                }
                None => s.parse::<u16>().ok().filter(|&n| n > 0).map(SizeSpec::Fixed),
            }
        }
        _ => None,
    }
}

/// Expands a leading `~` or `~/` with `$HOME` and joins relative paths onto `base`.
fn resolve_path(base: Option<&Path>, raw: &str) -> PathBuf {
    let path = expand_home(raw);
    match base {
        Some(base) if path.is_relative() => base.join(path),
        _ => path,
    }
}

fn expand_home(raw: &str) -> PathBuf {
    let rest = if raw == "~" { Some("") } else { raw.strip_prefix("~/") };
    match (rest, env::var_os("HOME")) {
        (Some(rest), Some(home)) if !home.is_empty() => {
            let home = PathBuf::from(home);
            if rest.is_empty() { home } else { home.join(rest) }
        }
        _ => PathBuf::from(raw),
    }
}

/// The effective cwd of a node: its own (resolved) or the inherited one.
fn inherit_cwd(base: Option<&Path>, raw: Option<&str>) -> Option<PathBuf> {
    match raw {
        Some(raw) => Some(resolve_path(base, raw)),
        None => base.map(Path::to_path_buf),
    }
}

/// Builds a split, collapsing it to its child when there is only one.
fn collapse(axis: Axis, mut children: Vec<SizedNode>) -> Option<LayoutNode> {
    match children.len() {
        0 => None,
        1 => children.pop().map(|(node, _)| node),
        _ => Some(LayoutNode::Split { axis, children }),
    }
}

/// The tiled root of a tab; an empty tab gets one shell pane, as in Zellij.
fn tab_root(axis: Axis, panes: Vec<SizedNode>, cwd: Option<&Path>) -> LayoutNode {
    collapse(axis, panes)
        .unwrap_or_else(|| LayoutNode::Pane(PaneDef { cwd: cwd.map(Path::to_path_buf), ..PaneDef::default() }))
}

struct Parser<'a> {
    src: &'a str,
    /// Warnings with the byte offset they refer to.
    warnings: Vec<(usize, String)>,
}

impl Parser<'_> {
    fn warn(&mut self, offset: usize, msg: impl fmt::Display) {
        let line = line_of(self.src, offset);
        self.warnings.push((offset, format!("line {line}: {msg}")));
    }

    fn unknown_entry(&mut self, entry: &KdlEntry, owner: &str) {
        let msg = match entry_key(entry) {
            Some(key) => format!("unsupported property `{key}` on `{owner}` ignored"),
            None => format!("unexpected argument {} on `{owner}` ignored", show(entry.value())),
        };
        self.warn(entry_pos(entry), msg);
    }

    fn unknown_node(&mut self, node: &KdlNode, owner: &str) {
        let name = node.name().value();
        self.warn(node_pos(node), format!("unsupported node `{name}` in `{owner}` ignored"));
    }

    fn string(&mut self, entry: &KdlEntry) -> Option<String> {
        let text = scalar_text(entry.value());
        if text.is_none() {
            let key = entry_key(entry).unwrap_or("value");
            self.warn(entry_pos(entry), format!("`{key}` must be a string, ignored"));
        }
        text
    }

    fn path(&mut self, entry: &KdlEntry) -> Option<String> {
        let text = self.string(entry)?;
        if text.is_empty() {
            let key = entry_key(entry).unwrap_or("path");
            self.warn(entry_pos(entry), format!("empty `{key}` ignored"));
            return None;
        }
        Some(text)
    }

    fn flag(&mut self, entry: &KdlEntry) -> Option<bool> {
        match entry.value() {
            KdlValue::Bool(b) => Some(*b),
            KdlValue::String(s) if s == "true" => Some(true),
            KdlValue::String(s) if s == "false" => Some(false),
            other => {
                let key = entry_key(entry).unwrap_or("value");
                self.warn(entry_pos(entry), format!("`{key}` must be true or false, found {}; ignored", show(other)));
                None
            }
        }
    }

    /// A boolean child node such as `close_on_exit true`; a bare node means true.
    fn flag_node(&mut self, node: &KdlNode) -> Option<bool> {
        match node.entries().first() {
            None => Some(true),
            Some(entry) => self.flag(entry),
        }
    }

    fn axis(&mut self, entry: &KdlEntry) -> Option<Axis> {
        let axis = entry.value().as_string().and_then(Axis::from_zellij);
        if axis.is_none() {
            self.warn(
                entry_pos(entry),
                format!(
                    "`split_direction` must be \"vertical\" or \"horizontal\", found {}; ignored",
                    show(entry.value())
                ),
            );
        }
        axis
    }

    fn size(&mut self, entry: &KdlEntry) -> Option<SizeSpec> {
        let size = parse_size(entry.value());
        if size.is_none() {
            self.warn(
                entry_pos(entry),
                format!(
                    "invalid `size` {} (expected cells like 10 or a percentage like \"50%\"); ignored",
                    show(entry.value())
                ),
            );
        }
        size
    }

    fn layout(&mut self, node: &KdlNode) -> (Option<PathBuf>, Vec<TabDef>) {
        // Zellij treats these as the implicit tab's properties when there are no tabs.
        let mut name = None;
        let mut axis = None;
        for entry in node.entries() {
            match entry_key(entry) {
                Some("name") => name = self.string(entry),
                Some("split_direction") => axis = self.axis(entry),
                Some("hide_floating_panes") => {}
                _ => self.unknown_entry(entry, "layout"),
            }
        }
        let items = child_nodes(node);

        // The workspace cwd applies to every tab, wherever it is declared.
        let mut cwd = None;
        for item in items.iter().filter(|item| item.name().value() == "cwd") {
            match item.entries().first() {
                Some(entry) if entry_key(entry).is_none() => {
                    if let Some(raw) = self.path(entry) {
                        cwd = Some(resolve_path(None, &raw));
                    }
                }
                _ => self.warn(node_pos(item), "`cwd` expects one path argument; ignored"),
            }
        }

        let mut tabs = Vec::new();
        let mut panes = Vec::new();
        let mut floating = Vec::new();
        let mut stray = None;
        for item in items {
            let before = (panes.len(), floating.len());
            if self.tab_item(item, cwd.as_deref(), &mut panes, &mut floating) {
                if stray.is_none() && (panes.len(), floating.len()) != before {
                    stray = Some(node_pos(item));
                }
                continue;
            }
            match item.name().value() {
                "tab" => tabs.push(self.tab(item, cwd.as_deref())),
                // Templates only carry Zellij's own plugin bars; swap layouts are
                // alternative arrangements that do not change the initial layout.
                "cwd" | "default_tab_template" | "new_tab_template" | "swap_tiled_layout" | "swap_floating_layout" => {}
                "pane_template" | "tab_template" => {
                    let kind = item.name().value();
                    self.warn(node_pos(item), format!("`{kind}` is not supported; ignored"));
                }
                _ => self.unknown_node(item, "layout"),
            }
        }

        if tabs.is_empty() {
            let root = tab_root(axis.unwrap_or_default(), panes, cwd.as_deref());
            tabs.push(TabDef { name, focus: false, cwd: cwd.clone(), root, floating });
        } else {
            if let Some(at) = stray {
                self.warn(at, "panes outside `tab` nodes are ignored when tabs exist");
            }
            if name.is_some() || axis.is_some() {
                self.warn(node_pos(node), "`name`/`split_direction` on `layout` are ignored when tabs exist");
            }
        }
        (cwd, tabs)
    }

    fn tab(&mut self, node: &KdlNode, ws_cwd: Option<&Path>) -> TabDef {
        let mut name = None;
        let mut focus = false;
        let mut cwd_raw = None;
        let mut axis = Axis::default();
        for entry in node.entries() {
            match entry_key(entry) {
                Some("name") => name = self.string(entry),
                Some("focus") => focus = self.flag(entry).unwrap_or(focus),
                Some("cwd") => cwd_raw = self.path(entry),
                Some("split_direction") => axis = self.axis(entry).unwrap_or(axis),
                Some("hide_floating_panes") => {}
                _ => self.unknown_entry(entry, "tab"),
            }
        }
        let cwd = inherit_cwd(ws_cwd, cwd_raw.as_deref());
        let mut panes = Vec::new();
        let mut floating = Vec::new();
        for item in child_nodes(node) {
            if !self.tab_item(item, cwd.as_deref(), &mut panes, &mut floating) {
                self.unknown_node(item, "tab");
            }
        }
        let root = tab_root(axis, panes, cwd.as_deref());
        TabDef { name, focus, cwd, root, floating }
    }

    /// Handles a node that may appear among a tab's panes; returns false for anything else.
    fn tab_item(
        &mut self,
        node: &KdlNode,
        cwd: Option<&Path>,
        panes: &mut Vec<SizedNode>,
        floating: &mut Vec<PaneDef>,
    ) -> bool {
        match node.name().value() {
            "pane" | "plugin" => panes.extend(self.pane(node, cwd)),
            "floating_panes" => {
                let more = self.floating_panes(node, cwd);
                floating.extend(more);
            }
            // Template placeholder; meaningless outside a template.
            "children" => {}
            _ => return false,
        }
        true
    }

    /// Parses a `pane` node. Returns `None` for plugin panes, which kraal drops.
    fn pane(&mut self, node: &KdlNode, parent_cwd: Option<&Path>) -> Option<SizedNode> {
        if node.name().value() == "plugin" {
            return None;
        }
        let mut def = PaneDef::default();
        let mut size = None;
        let mut axis = Axis::default();
        let mut cwd_raw = None;
        let mut preview = None;
        let mut plugin = false;
        let mut stacked = false;
        for entry in node.entries() {
            match entry_key(entry) {
                Some("name") => def.name = self.string(entry),
                Some("command") => def.command = self.string(entry),
                Some("cwd") => cwd_raw = self.path(entry),
                Some("focus") => def.focus = self.flag(entry).unwrap_or(def.focus),
                Some("size") => size = self.size(entry),
                Some("split_direction") => axis = self.axis(entry).unwrap_or(axis),
                Some("start_suspended") => {
                    def.start_suspended = self.flag(entry).unwrap_or(def.start_suspended);
                }
                Some("close_on_exit") => {
                    def.close_on_exit = self.flag(entry).unwrap_or(def.close_on_exit);
                }
                Some("preview") => preview = self.path(entry),
                Some("agent") => def.agent = self.string(entry),
                Some("account_id") => def.account_id = self.string(entry),
                Some("stacked") => stacked = self.flag(entry).unwrap_or(stacked),
                // Read by the stack holding the pane.
                Some("expanded") => {
                    self.flag(entry);
                }
                Some("plugin") => plugin = true,
                // Cosmetics and floating-pane geometry, which kraal does not model.
                Some("borderless" | "x" | "y" | "width" | "height" | "pinned") => {}
                _ => self.unknown_entry(entry, "pane"),
            }
        }
        let cwd = inherit_cwd(parent_cwd, cwd_raw.as_deref());

        let mut children = Vec::new();
        let mut container = false;
        let mut expanded = None;
        for item in child_nodes(node) {
            match item.name().value() {
                "pane" => {
                    container = true;
                    let before = children.len();
                    children.extend(self.pane(item, cwd.as_deref()));
                    if children.len() > before && expanded.is_none() && is_set(item, "expanded") {
                        expanded = Some(before);
                    }
                }
                "plugin" => plugin = true,
                "args" => self.args(item, &mut def.args),
                "start_suspended" => {
                    def.start_suspended = self.flag_node(item).unwrap_or(def.start_suspended);
                }
                "close_on_exit" => {
                    def.close_on_exit = self.flag_node(item).unwrap_or(def.close_on_exit);
                }
                "children" => {}
                _ => self.unknown_node(item, "pane"),
            }
        }

        if container {
            if def.command.is_some() || !def.args.is_empty() || preview.is_some() || def.agent.is_some() {
                self.warn(
                    node_pos(node),
                    "`command`, `args`, `preview` and `agent` on a pane with child panes are ignored",
                );
            }
            if stacked {
                return self.stack(node, children, expanded).map(|node| (node, size));
            }
            // A container whose children were all plugins disappears with them.
            return collapse(axis, children).map(|node| (node, size));
        }
        if plugin {
            return None;
        }
        // `args` go to the command, or to the agent kraal starts (`agent="claude" { args "--model" "opus" }`).
        if def.command.is_none() && def.agent.is_none() && !def.args.is_empty() {
            self.warn(node_pos(node), "`args` without `command` or `agent` ignored");
            def.args.clear();
        }
        if let Some(raw) = preview {
            if def.command.is_some() {
                self.warn(node_pos(node), "`command` on a preview pane ignored");
                def.command = None;
                def.args.clear();
            }
            def.kind = PaneKind::Preview(resolve_path(cwd.as_deref(), &raw));
        }
        def.cwd = cwd;
        Some((LayoutNode::Pane(def), size))
    }

    /// The stack of a `stacked=true` pane: its children, `expanded` being the index of the one
    /// shown whole. Panes nested deeper join it.
    fn stack(&mut self, node: &KdlNode, children: Vec<SizedNode>, expanded: Option<usize>) -> Option<LayoutNode> {
        let mut panes = Vec::new();
        let mut open = 0;
        for (i, (child, _)) in children.into_iter().enumerate() {
            if expanded == Some(i) {
                open = panes.len();
            }
            match child {
                LayoutNode::Pane(pane) => panes.push(pane),
                nested => {
                    self.warn(node_pos(node), "nested panes inside a stacked pane are flattened");
                    panes.extend(nested.into_panes());
                }
            }
        }
        match panes.len() {
            0 => None,
            1 => panes.pop().map(LayoutNode::Pane),
            _ => Some(LayoutNode::Stack { panes, expanded: open }),
        }
    }

    fn args(&mut self, node: &KdlNode, out: &mut Vec<String>) {
        for entry in node.entries() {
            if entry_key(entry).is_some() {
                self.unknown_entry(entry, "args");
            } else if let Some(arg) = self.string(entry) {
                out.push(arg);
            }
        }
    }

    fn floating_panes(&mut self, node: &KdlNode, cwd: Option<&Path>) -> Vec<PaneDef> {
        for entry in node.entries() {
            self.unknown_entry(entry, "floating_panes");
        }
        let mut out = Vec::new();
        for item in child_nodes(node) {
            match item.name().value() {
                "pane" | "plugin" => match self.pane(item, cwd) {
                    Some((LayoutNode::Pane(pane), _)) => out.push(pane),
                    Some((split, _)) => {
                        self.warn(node_pos(item), "nested panes inside a floating pane are flattened");
                        out.extend(split.into_panes());
                    }
                    None => {}
                },
                _ => self.unknown_node(item, "floating_panes"),
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Writing

const DEFAULT_TAB_TEMPLATE: &str = r#"default_tab_template {
    pane size=1 borderless=true {
        plugin location="zellij:tab-bar"
    }
    children
    pane size=1 borderless=true {
        plugin location="zellij:status-bar"
    }
}"#;

#[derive(Default)]
struct Writer {
    out: String,
}

impl Writer {
    fn line(&mut self, depth: usize, text: &str) {
        for _ in 0..depth {
            self.out.push_str("    ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn node(&mut self, node: &LayoutNode, size: Option<SizeSpec>, base: Option<&Path>, depth: usize) {
        match node {
            LayoutNode::Pane(pane) => self.pane(pane, size, base, depth, false),
            LayoutNode::Stack { panes, expanded } => {
                let mut head = String::from("pane stacked=true");
                push_size(&mut head, size);
                head.push_str(" {");
                self.line(depth, &head);
                for (i, pane) in panes.iter().enumerate() {
                    self.pane(pane, None, base, depth + 1, i == *expanded);
                }
                self.line(depth, "}");
            }
            LayoutNode::Split { axis, children } => {
                let mut head = String::from("pane");
                push_str(&mut head, "split_direction", Some(axis.zellij_name()));
                push_size(&mut head, size);
                head.push_str(" {");
                self.line(depth, &head);
                for (child, child_size) in children {
                    self.node(child, *child_size, base, depth + 1);
                }
                self.line(depth, "}");
            }
        }
    }

    fn pane(&mut self, pane: &PaneDef, size: Option<SizeSpec>, base: Option<&Path>, depth: usize, expanded: bool) {
        let mut head = String::from("pane");
        push_str(&mut head, "name", pane.name.as_deref());
        push_str(&mut head, "command", pane.command.as_deref());
        push_cwd(&mut head, pane.cwd.as_deref(), base);
        push_size(&mut head, size);
        push_flag(&mut head, "focus", pane.focus);
        push_flag(&mut head, "expanded", expanded);
        push_flag(&mut head, "start_suspended", pane.start_suspended);
        push_flag(&mut head, "close_on_exit", pane.close_on_exit);
        if let PaneKind::Preview(path) = &pane.kind {
            let own_cwd = pane.cwd.as_deref().or(base);
            push_str(&mut head, "preview", Some(&path_text(path, own_cwd)));
        }
        push_str(&mut head, "agent", pane.agent.as_deref());
        push_str(&mut head, "account_id", pane.account_id.as_deref());
        if pane.args.is_empty() {
            self.line(depth, &head);
            return;
        }
        head.push_str(" {");
        self.line(depth, &head);
        let args: Vec<String> = pane.args.iter().map(|arg| quote(arg)).collect();
        self.line(depth + 1, &format!("args {}", args.join(" ")));
        self.line(depth, "}");
    }
}

fn push_str(head: &mut String, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        let _ = write!(head, " {key}={}", quote(value));
    }
}

fn push_flag(head: &mut String, key: &str, value: bool) {
    if value {
        let _ = write!(head, " {key}=true");
    }
}

fn push_size(head: &mut String, size: Option<SizeSpec>) {
    match size {
        Some(SizeSpec::Fixed(n)) => {
            let _ = write!(head, " size={n}");
        }
        Some(SizeSpec::Percent(n)) => {
            let _ = write!(head, " size=\"{n}%\"");
        }
        None => {}
    }
}

/// Writes `cwd` unless it is simply inherited from `base`.
fn push_cwd(head: &mut String, cwd: Option<&Path>, base: Option<&Path>) {
    if let Some(cwd) = cwd
        && Some(cwd) != base
    {
        push_str(head, "cwd", Some(&path_text(cwd, base)));
    }
}

/// Text for a path that the parser will resolve against `base`: absolute paths are
/// written as-is, relative ones relative to `base`.
fn path_text(path: &Path, base: Option<&Path>) -> String {
    let shown = match base {
        Some(base) if path.is_relative() => path.strip_prefix(base).unwrap_or(path),
        _ => path,
    };
    if shown.as_os_str().is_empty() { ".".to_owned() } else { shown.to_string_lossy().into_owned() }
}

/// Quotes a string using escapes valid in both KDL v1 and v2.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            // Control, bidi-override and BOM characters must not appear literally in KDL v2.
            c if c.is_control()
                || matches!(
                    c,
                    '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
                ) =>
            {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests;
