use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use kdl::{KdlDocument, KdlEntry, KdlError, KdlNode, KdlValue};

use super::model::{Axis, LayoutNode, PaneDef, PaneKind, SizeSpec, TabDef, WorkspaceDef};

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
/// What to say when no workspace is named `name`: none is open under that name, and no file of
/// `dirs` defines one. When the name reads as a server's (`names_server`: someone typed `kraal
/// attach fpa@anvil`), it says how to attach to the kraal there, which is what was meant.
pub fn unknown(name: &str, dirs: &[PathBuf], names_server: bool) -> String {
    let dirs: Vec<String> = dirs.iter().map(|dir| crate::paths::shorten_home(dir)).collect();
    let plain = format!("no workspace named {name:?} in {}", dirs.join(", "));
    if names_server { format!("{plain}; to attach to kraal on that server: kraal attach --host {name}") } else { plain }
}
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
pub(super) fn parse_kdl(src: &str) -> Result<KdlDocument> {
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
