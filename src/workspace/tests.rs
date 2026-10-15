use std::path::{Path, PathBuf};
use std::{env, fs};

use super::*;

#[test]
fn workspace_name_rules() {
    for (name, used, expected) in [
        ("", false, Err(NameError::Empty)),
        ("  \u{2003}", true, Err(NameError::Empty)),
        ("\t\n", false, Err(NameError::Empty)),
        ("work", true, Err(NameError::Used)),
        ("work", false, Ok(())),
        ("My workspace 日本", false, Ok(())),
        (".hidden", false, Ok(())),
        (".", false, Ok(())),
        ("..", false, Ok(())),
        ("a/b", false, Ok(())),
        ("a\\b", false, Ok(())),
        (" work ", false, Ok(())),
    ] {
        assert_eq!(validate_name(name, used), expected, "{name:?}, used={used}");
    }
    for control in ('\0'..='\u{1f}').chain('\u{7f}'..='\u{9f}') {
        for name in [format!("{control}work"), format!("a{control}b"), format!("work{control}")] {
            for used in [false, true] {
                assert_eq!(validate_name(&name, used), Err(NameError::Control), "{name:?}");
            }
        }
    }
    assert_eq!(NameError::Empty.to_string(), "Type a name.");
    assert_eq!(NameError::Control.to_string(), "Choose a name without control characters.");
    assert_eq!(NameError::Used.to_string(), "This name is already used. Choose another name.");
}

#[test]
fn file_names_stay_in_their_directory() {
    for ok in ["work", "my workspace", "a.b"] {
        assert!(valid_file_name(ok), "{ok}");
    }
    for bad in ["", "..", ".", ".hidden", "../x", "a/b", "/etc/x", "a\\b", "a\nb", "tab\there"] {
        assert!(!valid_file_name(bad), "{bad:?}");
    }
    // A workspace's own name always gives a file name, distinct for distinct long names.
    let long = |end: &str| format!("prod: journalctl --no-pager --directory=/var/log/journal {}{end}", "x".repeat(300));
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

    let bad = parse("layout {\n    pane size=0\n    pane size=\"150%\"\n    pane size=1.5\n    pane size=70000\n}\n");
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
        assert_eq!(exported.panes(), [&PaneDef { agent: agent.map(str::to_owned), focus: true, ..Default::default() }]);
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
    for spelling in [linked.join("perso.kdl"), root.join("dotfiles/../config/./perso.kdl"), real.join("perso.kdl")] {
        assert_eq!(definition_path(&spelling), real.join("perso.kdl"), "{}", spelling.display());
    }
    assert_eq!(definition_path(&linked.join("alias.kdl")), real.join("alias.kdl"));
    // A file in a directory that is gone keeps the path it was given.
    assert_eq!(definition_path(&root.join("gone/x.kdl")), root.join("gone/x.kdl"));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn parses_local_zellij_layouts() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/workspace/fixtures/zellij");
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
