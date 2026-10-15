use super::model::{LayoutNode, PaneDef, PaneKind, SizeSpec, TabDef, WorkspaceDef};

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
