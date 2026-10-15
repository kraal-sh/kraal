use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use kdl::KdlDocument;

use super::model::{LayoutNode, PaneDef, PaneKind, SizeSpec, WorkspaceDef};
use super::read::parse_kdl;

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
