use std::fmt;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Where typed keys go besides the focused pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncScope {
    #[default]
    Off,
    /// Every terminal pane of the current tab.
    Tab,
    /// Every terminal pane of the workspace, all tabs.
    Workspace,
    /// The panes the user marked.
    Marked,
}

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
    pub(super) fn from_zellij(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("vertical") {
            Some(Self::Row)
        } else if s.eq_ignore_ascii_case("horizontal") {
            Some(Self::Column)
        } else {
            None
        }
    }

    pub(super) fn zellij_name(self) -> &'static str {
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

    pub(super) fn into_panes(self) -> Vec<PaneDef> {
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
