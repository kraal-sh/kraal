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

/// What [`valid_file_name`] accepts, said to the user.
pub const FILE_NAME_RULE: &str =
    "a workspace file name is one plain name: not empty, no \"/\" or control character, not starting with \".\"";

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

/// Bind old labels before a rename or removal, keeping the source formatting.
pub fn bind_accounts(path: &Path, accounts: &crate::accounts::Accounts) -> Result<()> {
    // Unrelated definitions may be read-only. Require a lock only when something needs binding,
    // then read again under it: a save may have replaced the definition since this first look.
    let source = fs::read_to_string(path)?;
    if bind_source_accounts(&source, accounts)? == source {
        return Ok(());
    }
    let target = fs::canonicalize(path)?;
    crate::paths::locked(&target, || {
        let source = fs::read_to_string(&target)?;
        let bound = bind_source_accounts(&source, accounts)?;
        if bound != source {
            crate::paths::write_atomic(&target, bound.as_bytes())?;
        }
        Ok(())
    })?
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
        parse_kdl(&source).context("checking migrated workspace accounts")?;
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
mod tests {
    use super::*;

    #[test]
    fn file_names_stay_in_their_directory() {
        for ok in ["work", "my workspace", "a.b"] {
            assert!(valid_file_name(ok), "{ok}");
        }
        for bad in ["", "..", ".", ".hidden", "../x", "a/b", "/etc/x", "a\\b", "a\nb", "tab\there"] {
            assert!(!valid_file_name(bad), "{bad:?}");
        }
        // A workspace's own name always gives a file name, distinct for distinct long names.
        let long =
            |end: &str| format!("prod: journalctl --no-pager --directory=/var/log/journal {}{end}", "x".repeat(300));
        for name in [
            "work",
            "prod: journalctl --directory=/var/log/journal",
            ".",
            "..",
            "",
            "a\nb",
            " .ops",
            "  ",
            " . ",
            "printf ok;\nsleep 600",
            &long("a"),
            &long("é"),
        ] {
            let file = file_name_from(name);
            assert!(valid_file_name(&file), "{name:?} gave {file:?}");
            assert!(!file.chars().any(char::is_control), "{name:?} gave {file:?}");
        }
        assert_eq!(file_name_from("work"), "work");
        assert!(file_name_from("prod: tail /var/log/x").starts_with("prod: tail _var_log_x-"));
        assert_ne!(file_name_from(&long("a")), file_name_from(&long("b")));
        // Names that differ only where they had to change still get files of their own, and
        // none of them takes the file of a workspace whose name needed no change.
        let names = ["prod: tail /var/log/a_b", "prod: tail /var/log/a/b", "prod: tail _var_log_a_b"];
        let files: std::collections::HashSet<String> = names.iter().map(|n| file_name_from(n)).collect();
        assert_eq!(files.len(), names.len(), "{files:?}");
    }

    const BARS: &str = r#"
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="tab-bar"
        }
        children
        pane size=1 borderless=true {
            plugin location="status-bar"
        }
    }
"#;

    /// Excerpt of a real fleet layout (the kind a Wave or Zellij user keeps).
    fn zellij_fleet() -> String {
        format!(
            r#"// Zellij layout transposed from Wave workspace 'Kandy'
layout {{
{BARS}
    tab name="lfimmo" {{
        pane split_direction="vertical" {{
            pane focus=true name="192.0.2.11" command="ssh" {{
                args "admin@192.0.2.11"
            }}
            pane split_direction="horizontal" {{
                pane name="192.0.2.12" command="ssh" {{
                    args "admin@192.0.2.12"
                }}
                pane name="192.0.2.13" command="ssh" {{
                    args "admin@192.0.2.13"
                }}
            }}
        }}
    }}
    tab name="main" focus=true {{
        pane split_direction="vertical" {{
            pane split_direction="horizontal" {{
                pane name="pve-main" command="ssh" {{
                    args "-p" "2222" "root@pve-main.example.net"
                }}
                pane focus=true name="edge-proxy" command="ssh" {{
                    args "deploy@edge-proxy.example.net"
                }}
            }}
            pane split_direction="horizontal" {{
                pane cwd="/home/me"
                pane name="nvr" command="ssh" {{
                    args "root@nvr.example.net"
                }}
            }}
            pane name="198.51.100.249" command="ssh" {{
                args "deploy@198.51.100.249"
            }}
        }}
    }}
    tab name="test" {{
        pane focus=true name="app-test" command="ssh" {{
            args "-t" "deploy@app-test.example.net" "cd /srv/website && exec $SHELL -l"
        }}
    }}
}}
"#
        )
    }

    #[test]
    fn account_bindings_keep_labels_comments_and_unknown_properties() {
        let dir = std::env::temp_dir().join(format!("kraal-workspace-account-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("work.kdl");
        let source = "// Mine.\nlayout {\n    pane agent=\"claude:default\" future=\"kept\" focus=true // account\n    pane agent=\"codex\"\n}\n";
        std::fs::write(&file, source).unwrap();
        let accounts = crate::accounts::Accounts::empty();
        bind_accounts(&file, &accounts).unwrap();
        let migrated = std::fs::read_to_string(&file).unwrap();
        assert_eq!(migrated, source.replace(" future=", " account_id=\"default-claude\" future="));
        let ws = load_file(&file).unwrap();
        let panes = ws.panes();
        assert_eq!(panes[0].agent.as_deref(), Some("claude:default"));
        assert_eq!(panes[0].account_id.as_deref(), Some("default-claude"));
        assert!(panes[1].account_id.is_none(), "an agent selected by rules stays dynamic");
        assert!(panes[0].focus, "the v1 boolean stays a boolean");
        let roundtrip = parse_workspace("work", &to_kdl(&ws)).unwrap();
        assert_eq!(roundtrip.panes()[0].account_id, panes[0].account_id);
        bind_accounts(&file, &accounts).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), migrated);
        std::fs::write(&file, source.replace("focus=true", "focus=#true")).unwrap();
        bind_accounts(&file, &accounts).unwrap();
        assert!(load_file(&file).unwrap().panes()[0].focus, "v2 stays v2");
        std::fs::write(&file, "layout {").unwrap();
        assert!(bind_accounts(&file, &accounts).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "layout {");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn account_bindings_use_the_last_agent_property() {
        let dir = std::env::temp_dir().join(format!("kraal-workspace-duplicate-account-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("work.kdl");
        let mut accounts = crate::accounts::Accounts::empty();
        let default = accounts.get("claude", "default").unwrap();
        for name in ["alpha", "beta"] {
            accounts
                .add(crate::accounts::Account {
                    id: name.into(),
                    name: name.into(),
                    home: format!("/{name}"),
                    ..default.clone()
                })
                .unwrap();
        }
        for (last, expected) in [("claude:beta", Some("beta")), ("codex", None)] {
            let source = format!("layout {{ pane agent=\"claude:alpha\" agent={last:?} }}\n");
            std::fs::write(&file, &source).unwrap();
            assert_eq!(load_file(&file).unwrap().panes()[0].agent.as_deref(), Some(last));
            bind_accounts(&file, &accounts).unwrap();
            let def = load_file(&file).unwrap();
            assert_eq!(def.panes()[0].agent.as_deref(), Some(last));
            assert_eq!(def.panes()[0].account_id.as_deref(), expected);
            if expected.is_none() {
                assert_eq!(std::fs::read_to_string(&file).unwrap(), source);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_only_definitions_without_legacy_bindings_need_no_lock() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kraal-readonly-workspace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shells.kdl");
        let source =
            "layout {\n    pane command=\"sh\"\n    pane agent=\"claude:default\" account_id=\"default-claude\"\n}\n";
        std::fs::write(&file, source).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = bind_accounts(&file, &crate::accounts::Accounts::empty());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), source);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no lock file was needed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn saves_and_account_migration_share_a_lock_and_reread() {
        use crate::paths;
        use std::sync::mpsc;
        let dir = std::env::temp_dir().join(format!("kraal-workspace-save-migration-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("work.kdl");
        let alias = dir.join("alias.kdl");
        std::os::unix::fs::symlink(&file, &alias).unwrap();
        let accounts = crate::accounts::Accounts::empty();
        let old = "layout { pane agent=\"claude:default\" }\n";
        std::fs::write(&file, old).unwrap();
        bind_accounts(&file, &accounts).unwrap();
        let migrated = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, old).unwrap();
        let saved = parse_workspace("work", "layout { pane name=\"new layout\" agent=\"codex:default\" }\n").unwrap();
        // Migration has read the old definition and holds its lock while a save arrives.
        let writer = paths::locked(&file, || {
            assert_eq!(std::fs::read_to_string(&file).unwrap(), old);
            let (tx, rx) = mpsc::channel();
            let (path, saved) = (alias.clone(), saved.clone());
            let expected = paths::FileRevision::read(&file).unwrap();
            let writer = std::thread::spawn(move || {
                // SAFETY: gettid has no preconditions.
                tx.send(unsafe { libc::gettid() }).unwrap();
                let error = save_file(&path, &saved, &expected).unwrap_err();
                assert!(format!("{error:#}").contains("changed while saving"), "{error:#}");
            });
            crate::testing::wait_asleep(rx.recv().unwrap());
            assert!(!writer.is_finished(), "the save must wait for migration's read/write operation");
            assert!(!file.with_extension("kdl.bak").exists(), "the backup also needs the lock");
            paths::write_atomic(&file, migrated.as_bytes()).unwrap();
            writer
        })
        .unwrap();
        writer.join().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), migrated);
        assert!(!file.with_extension("kdl.bak").exists());
        // A migration saw an old label before waiting for a save's lock. It must bind the
        // newly saved layout once that lock becomes available, keeping all its new panes.
        std::fs::write(&file, old).unwrap();
        let migration = paths::locked(&file, || {
            let (tx, rx) = mpsc::channel();
            let path = alias.clone();
            let migration = std::thread::spawn(move || {
                // SAFETY: gettid has no preconditions.
                tx.send(unsafe { libc::gettid() }).unwrap();
                bind_accounts(&path, &accounts).unwrap();
            });
            crate::testing::wait_asleep(rx.recv().unwrap());
            assert!(!migration.is_finished(), "migration must wait for the save");
            paths::write_atomic(&file, to_kdl(&saved).as_bytes()).unwrap();
            migration
        })
        .unwrap();
        migration.join().unwrap();
        let mut expected = saved;
        let LayoutNode::Pane(pane) = &mut expected.tabs[0].root else { panic!("pane") };
        pane.account_id = Some("default-codex".into());
        assert_eq!(load_file(&file).unwrap(), expected);
        assert!(std::fs::symlink_metadata(&alias).unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn parse(src: &str) -> WorkspaceDef {
        parse_workspace("test", src).expect("parses")
    }

    fn parse_clean(src: &str) -> WorkspaceDef {
        let ws = parse(src);
        assert!(ws.warnings.is_empty(), "unexpected warnings: {:?}", ws.warnings);
        ws
    }

    fn leaf(node: &LayoutNode) -> &PaneDef {
        match node {
            LayoutNode::Pane(pane) => pane,
            _ => panic!("expected a pane, got {node:?}"),
        }
    }

    fn split(node: &LayoutNode) -> (Axis, &[(LayoutNode, Option<SizeSpec>)]) {
        match node {
            LayoutNode::Split { axis, children } => (*axis, children),
            _ => panic!("expected a split, got {node:?}"),
        }
    }

    fn assert_round_trip(ws: &WorkspaceDef) {
        let text = to_kdl(ws);
        let again = parse_workspace(&ws.name, &text).unwrap_or_else(|e| panic!("re-parse failed: {e:#}\n{text}"));
        assert_eq!(&again, ws, "round trip changed the workspace:\n{text}");
    }

    #[test]
    fn parses_zellij_fleet_layout() {
        let ws = parse_clean(&zellij_fleet());
        assert_eq!(ws.name, "test");
        assert_eq!(ws.cwd, None);
        let names: Vec<_> = ws.tabs.iter().map(|t| t.name.as_deref()).collect();
        assert_eq!(names, [Some("lfimmo"), Some("main"), Some("test")]);
        let focus: Vec<_> = ws.tabs.iter().map(|t| t.focus).collect();
        assert_eq!(focus, [false, true, false]);

        // Zellij "vertical" places children side by side.
        let (axis, children) = split(&ws.tabs[0].root);
        assert_eq!(axis, Axis::Row);
        assert_eq!(children.len(), 2);
        let first = leaf(&children[0].0);
        assert_eq!(first.name.as_deref(), Some("192.0.2.11"));
        assert_eq!(first.command.as_deref(), Some("ssh"));
        assert_eq!(first.args, ["admin@192.0.2.11"]);
        assert!(first.focus);
        assert_eq!(first.kind, PaneKind::Terminal);
        let (inner_axis, inner) = split(&children[1].0);
        assert_eq!(inner_axis, Axis::Column);
        assert_eq!(inner.len(), 2);
        assert!(!leaf(&inner[0].0).focus);
        assert_eq!(ws.tabs.iter().map(|t| t.panes().len()).sum::<usize>(), 9);
    }

    #[test]
    fn keeps_multi_value_args() {
        let ws = parse_clean(&zellij_fleet());
        let (_, columns) = split(&ws.tabs[1].root);
        let (_, top) = split(&columns[0].0);
        assert_eq!(leaf(&top[0].0).args, ["-p", "2222", "root@pve-main.example.net"]);
        let pool = leaf(&ws.tabs[2].root);
        assert_eq!(pool.args, ["-t", "deploy@app-test.example.net", "cd /srv/website && exec $SHELL -l"]);
    }

    #[test]
    fn single_pane_tab_is_the_root() {
        let ws = parse_clean(&zellij_fleet());
        let pool = leaf(&ws.tabs[2].root);
        assert_eq!(pool.name.as_deref(), Some("app-test"));
        assert!(pool.focus);
    }

    #[test]
    fn bare_pane_is_a_shell() {
        let ws = parse_clean(&zellij_fleet());
        let (_, columns) = split(&ws.tabs[1].root);
        let (_, middle) = split(&columns[1].0);
        let shell = leaf(&middle[0].0);
        assert_eq!(shell, &PaneDef { cwd: Some(PathBuf::from("/home/me")), ..PaneDef::default() });
        let src = "layout {\n    tab name=\"empty\"\n    tab { pane; }\n}\n";
        let ws = parse_clean(src);
        assert_eq!(ws.tabs[0].root, LayoutNode::Pane(PaneDef::default()));
        assert_eq!(ws.tabs[1].root, LayoutNode::Pane(PaneDef::default()));
    }

    #[test]
    fn panes_without_tabs_form_one_tab() {
        let src = r#"
layout {
    pane size=1 borderless=true {
        plugin location="zellij:tab-bar"
    }
    pane command="htop"
    pane command="btop" focus=true
    floating_panes {
        pane command="lazygit" x=10 y="10%" width="80%" height=20
    }
}
"#;
        let ws = parse_clean(src);
        assert_eq!(ws.tabs.len(), 1);
        let tab = &ws.tabs[0];
        assert_eq!(tab.name, None);
        let (axis, children) = split(&tab.root);
        assert_eq!(axis, Axis::Column);
        assert_eq!(children.len(), 2);
        assert!(leaf(&children[1].0).focus);
        assert_eq!(tab.floating.len(), 1);
        assert_eq!(tab.floating[0].command.as_deref(), Some("lazygit"));

        let ws = parse_clean("layout {\n}\n");
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs[0].root, LayoutNode::Pane(PaneDef::default()));

        let ws = parse_clean("layout name=\"solo\" split_direction=\"vertical\" {\n    pane\n    pane\n}\n");
        assert_eq!(ws.tabs[0].name.as_deref(), Some("solo"));
        assert_eq!(split(&ws.tabs[0].root).0, Axis::Row);
    }

    #[test]
    fn parses_sizes() {
        let src = r#"
layout {
    tab split_direction="vertical" {
        pane size=30
        pane size="25%"
        pane size=" 40 % "
        pane
    }
}
"#;
        let ws = parse_clean(src);
        let (axis, children) = split(&ws.tabs[0].root);
        assert_eq!(axis, Axis::Row);
        let sizes: Vec<_> = children.iter().map(|(_, size)| *size).collect();
        assert_eq!(sizes, [Some(SizeSpec::Fixed(30)), Some(SizeSpec::Percent(25)), Some(SizeSpec::Percent(40)), None]);

        let bad =
            parse("layout {\n    pane size=0\n    pane size=\"150%\"\n    pane size=1.5\n    pane size=70000\n}\n");
        assert_eq!(bad.warnings.len(), 4, "{:?}", bad.warnings);
        assert!(bad.warnings.iter().all(|w| w.contains("invalid `size`")));
        let (_, children) = split(&bad.tabs[0].root);
        assert!(children.iter().all(|(_, size)| size.is_none()));
    }

    #[test]
    fn collapses_single_child_splits() {
        let src = r#"
layout {
    tab {
        pane split_direction="vertical" {
            pane size="30%" split_direction="horizontal" {
                pane size=5 split_direction="vertical" {
                    pane command="htop"
                }
            }
            pane command="top"
        }
    }
}
"#;
        let ws = parse_clean(src);
        let (axis, children) = split(&ws.tabs[0].root);
        assert_eq!(axis, Axis::Row);
        // The chain of one-child containers collapses to the leaf, keeping the outer size.
        assert_eq!(leaf(&children[0].0).command.as_deref(), Some("htop"));
        assert_eq!(children[0].1, Some(SizeSpec::Percent(30)));
    }

    #[test]
    fn drops_plugin_panes() {
        let src = r#"
layout {
    tab {
        pane size=1 borderless=true {
            plugin location="zellij:tab-bar"
        }
        pane command="htop"
        pane split_direction="vertical" {
            pane { plugin location="zellij:strider"; }
            plugin location="zellij:compact-bar"
        }
        pane plugin="zellij:status-bar"
    }
    tab {
        pane { plugin location="zellij:session-manager"; }
    }
}
"#;
        let ws = parse_clean(src);
        assert_eq!(leaf(&ws.tabs[0].root).command.as_deref(), Some("htop"));
        // A tab with nothing but plugins still gets a shell, as in Zellij.
        assert_eq!(ws.tabs[1].root, LayoutNode::Pane(PaneDef::default()));
    }

    #[test]
    fn parses_floating_panes() {
        let src = r#"
layout {
    tab name="dev" cwd="/srv" {
        pane command="nvim"
        floating_panes {
            pane command="lazygit" width="80%" height="80%" x="10%" y="10%" pinned=true
            pane cwd="logs" name="tail" command="tail" close_on_exit=true {
                args "-f" "app.log"
            }
            pane { plugin location="zellij:filepicker"; }
        }
    }
}
"#;
        let ws = parse_clean(src);
        let tab = &ws.tabs[0];
        assert_eq!(tab.floating.len(), 2);
        assert_eq!(tab.floating[0].cwd, Some(PathBuf::from("/srv")));
        assert_eq!(tab.floating[1].cwd, Some(PathBuf::from("/srv/logs")));
        assert!(tab.floating[1].close_on_exit);
        let commands: Vec<_> = ws.panes().iter().map(|p| p.command.clone()).collect();
        assert_eq!(commands, [Some("nvim".to_owned()), Some("lazygit".to_owned()), Some("tail".to_owned())]);
    }

    #[test]
    fn suspend_and_close_flags_as_properties_or_children() {
        let src = r#"
layout {
    pane command="a" start_suspended=true close_on_exit=false
    pane command="b" {
        start_suspended true
        close_on_exit true
    }
    pane command="c" {
        close_on_exit
    }
}
"#;
        let ws = parse_clean(src);
        let flags: Vec<_> = ws.panes().iter().map(|p| (p.start_suspended, p.close_on_exit)).collect();
        assert_eq!(flags, [(true, false), (true, true), (false, true)]);
    }

    #[test]
    fn kraal_extensions() {
        let src = r#"
layout {
    tab name="agents" cwd="/work/kraal" {
        pane split_direction="vertical" {
            pane name="claude" command="claude" agent="claude" focus=true
            pane name="plan" preview="docs/PLAN.md" size="40%"
            pane preview="/etc/motd"
        }
    }
}
"#;
        let ws = parse_clean(src);
        let panes = ws.panes();
        assert_eq!(panes[0].agent.as_deref(), Some("claude"));
        assert_eq!(panes[0].kind, PaneKind::Terminal);
        assert_eq!(panes[1].kind, PaneKind::Preview(PathBuf::from("/work/kraal/docs/PLAN.md")));
        assert_eq!(panes[1].command, None);
        assert_eq!(panes[2].kind, PaneKind::Preview(PathBuf::from("/etc/motd")));

        let ws = parse("layout {\n    pane preview=\"a.md\" command=\"cat\" {\n        args \"x\"\n    }\n}\n");
        let pane = leaf(&ws.tabs[0].root);
        assert_eq!(pane.kind, PaneKind::Preview(PathBuf::from("a.md")));
        assert_eq!((pane.command.as_deref(), pane.args.len()), (None, 0));
        assert_eq!(ws.warnings.len(), 1, "{:?}", ws.warnings);
    }

    #[test]
    fn resolves_cwd_chain() {
        let src = r#"
layout {
    tab name="rel" cwd="projects" {
        pane split_direction="vertical" cwd="kraal" {
            pane cwd="src"
            pane cwd="/tmp"
            pane
        }
    }
    tab name="abs" cwd="/opt" {
        pane
    }
    tab name="inherit" {
        pane cwd="notes"
    }
    cwd "/home/me"
}
"#;
        let ws = parse_clean(src);
        assert_eq!(ws.cwd, Some(PathBuf::from("/home/me")));
        assert_eq!(ws.tabs[0].cwd, Some(PathBuf::from("/home/me/projects")));
        let cwds: Vec<_> = ws.tabs[0].panes().iter().map(|p| p.cwd.clone()).collect();
        assert_eq!(
            cwds,
            [
                Some(PathBuf::from("/home/me/projects/kraal/src")),
                Some(PathBuf::from("/tmp")),
                Some(PathBuf::from("/home/me/projects/kraal")),
            ]
        );
        assert_eq!(ws.tabs[1].cwd, Some(PathBuf::from("/opt")));
        assert_eq!(leaf(&ws.tabs[1].root).cwd, Some(PathBuf::from("/opt")));
        assert_eq!(ws.tabs[2].cwd, Some(PathBuf::from("/home/me")));
        assert_eq!(leaf(&ws.tabs[2].root).cwd, Some(PathBuf::from("/home/me/notes")));
    }

    #[test]
    fn expands_home() {
        let ws = parse_clean(
            "layout {\n    tab cwd=\"/elsewhere\" {\n        pane cwd=\"~/code\"\n        pane cwd=\"~\"\n        pane cwd=\"~other/x\"\n    }\n}\n",
        );
        let cwds: Vec<_> = ws.panes().iter().map(|p| p.cwd.clone().unwrap()).collect();
        match env::var_os("HOME").filter(|h| !h.is_empty()) {
            Some(home) => {
                let home = PathBuf::from(home);
                assert_eq!(cwds[0], home.join("code"));
                assert_eq!(cwds[1], home);
            }
            None => assert_eq!(cwds[0], PathBuf::from("/elsewhere/~/code")),
        }
        // Only the current user's home is expanded.
        assert_eq!(cwds[2], PathBuf::from("/elsewhere/~other/x"));
    }

    #[test]
    fn accepts_kdl_v2() {
        let src = r##"
layout {
    tab name=main focus=#true split_direction=vertical {
        pane command=htop size="30%" close_on_exit=#true
        pane focus=#true command=ls {
            args -la "/tmp"
        }
    }
    tab name=#"raw "quoted""#
}
"##;
        let ws = parse_clean(src);
        assert!(ws.tabs[0].focus);
        let (axis, children) = split(&ws.tabs[0].root);
        assert_eq!(axis, Axis::Row);
        assert_eq!(children[0].1, Some(SizeSpec::Percent(30)));
        assert!(leaf(&children[0].0).close_on_exit);
        let ls = leaf(&children[1].0);
        assert!(ls.focus);
        assert_eq!(ls.args, ["-la", "/tmp"]);
        assert_eq!(ws.tabs[1].name.as_deref(), Some("raw \"quoted\""));
    }

    #[test]
    fn warns_on_unknown_constructs() {
        let src = r#"layout {
    mystery 1
    pane_template name="ssh" {
        pane command="ssh"
    }
    tab name="t" color="red" {
        pane command="ls" sticky=true {
            args "-l"
            bogus
        }
        pane "positional"
        pane focus="maybe"
        pane split_direction="diagonal"
        pane { args "orphan"; }
        what
    }
}
keybinds {
}
"#;
        let ws = parse(src);
        let expected = [
            "line 2: unsupported node `mystery` in `layout` ignored",
            "line 3: `pane_template` is not supported; ignored",
            "line 6: unsupported property `color` on `tab` ignored",
            "line 7: unsupported property `sticky` on `pane` ignored",
            "line 9: unsupported node `bogus` in `pane` ignored",
            "line 11: unexpected argument \"positional\" on `pane` ignored",
            "line 12: `focus` must be true or false, found \"maybe\"; ignored",
            "line 13: `split_direction` must be \"vertical\" or \"horizontal\", found \"diagonal\"; ignored",
            "line 14: `args` without `command` or `agent` ignored",
            "line 15: unsupported node `what` in `tab` ignored",
            "line 18: unknown top-level node `keybinds` ignored",
        ];
        assert_eq!(ws.warnings, expected);
        // Everything that could be understood is still there.
        assert_eq!(ws.panes().len(), 5);
        assert_eq!(ws.panes()[0].args, ["-l"]);
    }

    #[test]
    fn warns_on_panes_outside_tabs() {
        let src = "layout {\n    pane command=\"stray\"\n    tab name=\"a\"\n}\n";
        let ws = parse(src);
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.panes()[0].command, None);
        assert_eq!(ws.warnings, ["line 2: panes outside `tab` nodes are ignored when tabs exist"]);
    }

    #[test]
    fn rejects_invalid_files() {
        let err = parse_workspace("x", "layout {\n    pane name=\"a\n}\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.starts_with("invalid KDL: line "), "{msg}");

        let err = parse_workspace("x", "layout {\n    tab {\n        pane focus=true\n    \n").unwrap_err();
        assert!(format!("{err:#}").contains("invalid KDL"));

        let err = parse_workspace("x", "tab name=\"a\"\n").unwrap_err();
        assert_eq!(err.to_string(), "no `layout` node");
    }

    #[test]
    fn round_trips_zellij_layout() {
        let ws = parse_clean(&zellij_fleet());
        assert_round_trip(&ws);
        let text = to_kdl(&ws);
        // Plain Zellij input must produce plain Zellij output.
        assert!(!text.contains("preview=") && !text.contains("agent="));
        assert!(text.contains("args \"-p\" \"2222\" \"root@pve-main.example.net\""));
        assert!(text.contains("default_tab_template"));
    }

    #[test]
    fn exporting_a_detected_shell_agent_keeps_only_its_kind_as_a_launch_recipe() {
        for (argv, agent) in [
            (vec!["claude".into(), "--model".into(), "opus".into()], Some("claude")),
            (vec!["sleep".into(), "600".into()], None),
        ] {
            let detected = crate::agent::detect_agent(&argv);
            assert_eq!(detected, agent);
            let snap: crate::persist::WorkspaceSnap = serde_json::from_value(serde_json::json!({
                "name": "shell",
                "definition": null,
                "cwd": null,
                "active": 0,
                "tabs": [{
                    "name": "shell",
                    "root": {"Pane": crate::persist::PaneSnap {
                        name: None,
                        spec: crate::pane::PaneSpec {
                            env: vec![("SHELL_ONLY".into(), "original-environment".into())],
                            ..Default::default()
                        },
                        agent: detected.map(str::to_owned),
                        agent_session: agent.map(|_| "original-conversation".into()),
                        stopped: false,
                        scrollback: None,
                    }},
                    "floating": [],
                    "focus": 0,
                    "show_floating": false,
                    "zoomed": false,
                    "sync": false
                }]
            }))
            .unwrap();
            let mut def = from_snapshot(&snap, "exported");
            def.portable_accounts();
            let text = to_kdl(&def);
            let exported = parse_clean(&text);
            assert_eq!(
                exported.panes(),
                [&PaneDef { agent: agent.map(str::to_owned), focus: true, ..Default::default() }]
            );
            for omitted in ["--model", "opus", "SHELL_ONLY", "original-environment", "original-conversation"] {
                assert!(!text.contains(omitted), "{text}");
            }
        }
    }

    #[test]
    fn round_trips_every_feature() {
        let src = r#"
layout {
    cwd "/home/me"
    tab name="agents" cwd="work" focus=true {
        pane split_direction="vertical" {
            pane name="claude" command="claude" agent="claude" focus=true size="60%"
            pane split_direction="horizontal" size=40 cwd="docs" {
                pane preview="PLAN.md" size=10
                pane preview="/etc/motd"
                pane command="cargo" start_suspended=true close_on_exit=true cwd="/tmp" {
                    args "watch" "-x" "test"
                }
            }
        }
        floating_panes {
            pane command="lazygit" cwd="sub" focus=true
            pane agent="claude:pro" {
                args "--model" "opus"
            }
        }
    }
    tab name="fleet" {
        pane command="ssh" name="web" {
            args "-p" "44022" "root@web"
        }
    }
}
"#;
        let ws = parse_clean(src);
        assert_round_trip(&ws);
        // An agent keeps the arguments it was saved with.
        let agent = ws.panes().into_iter().find(|p| p.agent.as_deref() == Some("claude:pro")).unwrap();
        assert_eq!(agent.args, ["--model", "opus"]);
    }

    #[test]
    fn round_trips_relative_cwds_and_odd_strings() {
        let ws = WorkspaceDef {
            name: "odd".to_owned(),
            cwd: Some(PathBuf::from("projects")),
            tabs: vec![
                TabDef {
                    name: Some("quote \" back \\ nl \n tab \t bell \u{7} rtl \u{202e} é 🦀".to_owned()),
                    focus: false,
                    cwd: Some(PathBuf::from("projects/kraal")),
                    root: LayoutNode::Split {
                        axis: Axis::Row,
                        children: vec![
                            (
                                LayoutNode::Pane(PaneDef {
                                    command: Some("sh".to_owned()),
                                    args: vec!["-c".to_owned(), "echo \"$HOME\" && true".to_owned(), String::new()],
                                    cwd: Some(PathBuf::from("projects/kraal/src")),
                                    kind: PaneKind::Terminal,
                                    ..PaneDef::default()
                                }),
                                Some(SizeSpec::Fixed(80)),
                            ),
                            (
                                LayoutNode::Pane(PaneDef {
                                    cwd: Some(PathBuf::from("projects/kraal")),
                                    kind: PaneKind::Preview(PathBuf::from("projects/kraal")),
                                    ..PaneDef::default()
                                }),
                                None,
                            ),
                        ],
                    },
                    floating: vec![],
                },
                TabDef {
                    name: None,
                    focus: true,
                    cwd: Some(PathBuf::from("projects")),
                    root: LayoutNode::Pane(PaneDef { cwd: Some(PathBuf::from("projects")), ..PaneDef::default() }),
                    floating: vec![],
                },
            ],
            warnings: vec![],
        };
        assert_round_trip(&ws);
    }

    #[test]
    fn serde_round_trip() {
        let ws = parse_clean(&zellij_fleet());
        let json = serde_json::to_string(&ws).unwrap();
        let back: WorkspaceDef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ws);
    }

    #[test]
    fn load_dir_sorts_and_reports_errors() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("kraal-ws-test-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("zeta.kdl"), "layout {\n    pane\n}\n").unwrap();
        fs::write(dir.join("alpha.kdl"), zellij_fleet()).unwrap();
        fs::write(dir.join("broken.kdl"), "layout {\n").unwrap();
        fs::write(dir.join("notes.txt"), "not a layout").unwrap();

        let loaded = load_dir(&dir);
        let names: Vec<_> =
            loaded.iter().map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["alpha.kdl", "broken.kdl", "zeta.kdl"]);
        assert_eq!(loaded[0].1.as_ref().unwrap().name, "alpha");
        let err = loaded[1].1.as_ref().unwrap_err();
        assert!(format!("{err:#}").contains("broken.kdl"), "{err:#}");
        assert_eq!(loaded[2].1.as_ref().unwrap().tabs.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
        assert!(load_dir(&dir).is_empty());
    }

    /// Through a linked directory or another spelling of it, a definition has the path
    /// `load_dir` lists; a definition that is a link keeps its own.
    #[test]
    fn a_definition_has_one_path_however_its_directory_is_reached() {
        let root = env::temp_dir().join(format!("kraal-ws-paths-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (real, linked) = (root.join("dotfiles/workspaces"), root.join("config"));
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        fs::write(real.join("perso.kdl"), "layout {\n    pane\n}\n").unwrap();
        std::os::unix::fs::symlink(real.join("perso.kdl"), real.join("alias.kdl")).unwrap();

        let listed: Vec<PathBuf> = load_dir(&linked).into_iter().map(|(path, _)| path).collect();
        let real = fs::canonicalize(&real).unwrap();
        assert_eq!(listed, [real.join("alias.kdl"), real.join("perso.kdl")]);
        for spelling in [linked.join("perso.kdl"), root.join("dotfiles/../config/./perso.kdl"), real.join("perso.kdl")]
        {
            assert_eq!(definition_path(&spelling), real.join("perso.kdl"), "{}", spelling.display());
        }
        assert_eq!(definition_path(&linked.join("alias.kdl")), real.join("alias.kdl"));
        // A file in a directory that is gone keeps the path it was given.
        assert_eq!(definition_path(&root.join("gone/x.kdl")), root.join("gone/x.kdl"));
        fs::remove_dir_all(&root).unwrap();
    }

    /// Parses the real layouts in `~/.config/zellij/layouts` (run with `--ignored`).
    #[test]
    #[ignore = "depends on the local Zellij configuration"]
    fn parses_local_zellij_layouts() {
        let home = env::var_os("HOME").expect("HOME is set");
        let dir = PathBuf::from(home).join(".config/zellij/layouts");
        let loaded = load_dir(&dir);
        assert!(!loaded.is_empty(), "no layouts in {}", dir.display());
        for (path, ws) in loaded {
            let ws = ws.unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
            assert!(ws.warnings.is_empty(), "{}: {:?}", path.display(), ws.warnings);
            assert_round_trip(&ws);
            println!("{}: {} tabs, {} panes", ws.name, ws.tabs.len(), ws.panes().len());
        }
    }

    #[test]
    fn stacked_panes() {
        let src = r#"
layout {
    pane split_direction="vertical" {
        pane name="editor" size="60%"
        pane stacked=true {
            pane name="logs"
            pane name="tests" expanded=true
            pane name="shell" command="htop"
        }
    }
}
"#;
        let ws = parse_clean(src);
        let (_, children) = split(&ws.tabs[0].root);
        let LayoutNode::Stack { panes, expanded } = &children[1].0 else { panic!("{:?}", children[1]) };
        let names: Vec<_> = panes.iter().map(|p| p.name.as_deref().unwrap_or_default()).collect();
        assert_eq!((names, *expanded), (vec!["logs", "tests", "shell"], 1));
        assert_eq!(ws.panes().len(), 4);
        assert_round_trip(&ws);
        let text = to_kdl(&ws);
        assert!(text.contains("pane stacked=true {"), "{text}");
        assert!(text.contains("pane name=\"tests\" expanded=true"), "{text}");
        assert!(!text.contains("name=\"logs\" expanded"), "{text}");

        // Without a pane marked, the first one is open; deeper panes join the stack; a stack of
        // one pane is that pane.
        let mut ws = parse(
            r#"
layout {
    pane stacked=true {
        pane name="a"
        pane split_direction="vertical" {
            pane name="b"
            pane name="c" expanded=true
        }
    }
    pane stacked=true {
        pane name="d"
    }
}
"#,
        );
        assert_eq!(ws.warnings.len(), 1, "{:?}", ws.warnings);
        assert!(ws.warnings[0].contains("flattened"), "{:?}", ws.warnings);
        let (axis, children) = split(&ws.tabs[0].root);
        assert_eq!(axis, Axis::Column);
        let LayoutNode::Stack { panes, expanded } = &children[0].0 else { panic!("{:?}", children[0]) };
        assert_eq!((panes.len(), *expanded), (3, 0));
        assert_eq!(leaf(&children[1].0).name.as_deref(), Some("d"));
        ws.warnings.clear();
        assert_round_trip(&ws);
    }

    /// Workspace files as users write them, damaged any way: read without a panic, and what is read
    /// written back reads the same.
    #[test]
    fn fuzz_workspace_files() {
        let samples = [
            zellij_fleet(),
            "layout {\n    pane split_direction=\"vertical\" {\n        pane size=\"30%\" command=\"htop\"\n        pane stacked=true {\n            pane name=\"a\" cwd=\"~/x\"\n            pane name=\"b\" expanded=true\n        }\n    }\n}\n".to_string(),
            "layout {\n    cwd \"/srv\"\n    tab name=\"t\" focus=true {\n        pane size=3 borderless=true\n        pane split_direction=\"horizontal\" { pane; pane; }\n    }\n    floating_panes { pane x=1 y=\"10%\" width=40 height=\"50%\" }\n}\n".to_string(),
        ];
        let tokens: &[&[u8]] = &[
            b"pane ",
            b"{",
            b"}",
            b"\n",
            b"size=",
            b"\"",
            b"=",
            b"split_direction=\"vertical\"",
            b"stacked=true",
            b"tab ",
            b"layout {",
            b"cwd=\"..\"",
            b"r#\"",
            b"#\"",
            b"\"#",
            b"\"\"\"",
            b"\"}\"",
            b"/*",
            b"*/",
            b"//",
            b"\\",
            b"/-",
            b"/-pane {",
            b";",
            b"100%",
            b"0",
            b"-1",
            b"999999999999",
        ];
        crate::fuzz::run("workspace_files", 100, |rng| {
            let mut text = rng.pick(&samples).clone().into_bytes();
            for _ in 0..rng.range(1, 4) {
                rng.mutate(&mut text, tokens);
            }
            let text = String::from_utf8_lossy(&text);
            crate::fuzz::trace(|| format!("{text:?}"));
            if let Ok(mut ws) = parse_workspace("fuzz", &text) {
                let _ = ws.panes();
                let written = to_kdl(&ws);
                let again = parse_workspace("fuzz", &written)
                    .unwrap_or_else(|e| panic!("what was written does not read: {e:#}\n{written}\nfrom\n{text}"));
                // What was not understood is said once, when read, and not written back.
                ws.warnings.clear();
                assert_eq!(again, ws, "written back:\n{written}\nfrom\n{text}");
            }
        });
    }

    /// A file one of the parsers panics on is an error, not a panic of the server that reads it.
    #[test]
    fn a_file_a_parser_panics_on_does_not_read() {
        let err = parse_workspace("broken", "layout {\n(true\n").unwrap_err();
        assert!(err.to_string().starts_with("invalid KDL"), "{err}");
    }

    /// A file nesting blocks thousands deep is refused, not read until the stack overflows (on a
    /// thread kraal names, whose stack is the smallest it reads a file on), braces in its strings
    /// and comments whatever they say.
    #[test]
    fn deeply_nested_blocks_are_refused() {
        crate::threads::named("nested")
            .spawn(|| {
                // A brace closed in a string or a comment closes nothing.
                for open in
                    ["pane {", "pane \"}\" {", "pane r#\"}\"# {", "pane #\"}\"# {", "pane /* } */ {", "pane { // }\n"]
                {
                    let deep = format!("layout {{ {} }}", open.repeat(5000) + &"} ".repeat(5000));
                    let err = parse_workspace("deep", &deep).unwrap_err();
                    assert!(err.to_string().contains("nested"), "{open}: {err}");
                }
                let fine = format!(
                    "layout {{ {} }}",
                    "pane split_direction=\"vertical\" { ".repeat(20) + "pane" + &" }".repeat(20)
                );
                assert!(parse_workspace("fine", &fine).is_ok());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// Blocks commented out (`/-`) within blocks commented out take both parsers a time that
    /// doubles with each: past a few, the file is refused at once. Layouts have a few, beside
    /// each other more than within.
    #[test]
    fn blocks_commented_out_deep_within_each_other_are_refused() {
        let deep = format!("layout {{\n{}}}\n", "/-pane {\n    pane\n".repeat(24) + &"}\n".repeat(24));
        let err = parse_workspace("deep", &deep).unwrap_err();
        assert!(err.to_string().contains("commented out"), "{err}");
        // Behind a string left open: v1 reads it on to the end, v2 to the line's end, and v2 reads
        // on past the error.
        for open in ["tab name=\"x {", "tab name=\"\"\" {"] {
            let hidden = format!("layout {{\n{open}\n{}", "/-pane {\n    pane\n".repeat(24));
            let err = parse_workspace("hidden", &hidden).unwrap_err();
            assert!(err.to_string().contains("commented out"), "{open}: {err}");
        }
        let fine = r#"
layout {
    /-tab name="old" {
        pane
    }
    tab {
        /-pane command="x"
        pane split_direction="vertical" {
            /-pane {
                /-pane {
                    pane
                }
            }
            pane cwd="/-x" {
                pane
                /-pane
            }
            pane
        }
    }
}
"#;
        assert!(parse_workspace("fine", fine).is_ok(), "{:?}", parse_workspace("fine", fine));
    }
}
