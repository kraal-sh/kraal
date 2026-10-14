//! Temporary 0.4 session migration. See `accounts::migration` for the 0.5.0 removal boundary.

use super::*;

pub(super) fn read(path: &Path, data: &[u8], to: &Path, scrollback: &Path, convert: bool) -> Result<SessionSnapshot> {
    let (snap, updated) = migrate(data)?;
    if !convert {
        return Ok(serde_json::from_slice(data)?);
    }
    convert_from(path, data, &snap, &updated, to, scrollback)?;
    Ok(snap)
}

fn convert_from(
    path: &Path,
    data: &[u8],
    snap: &SessionSnapshot,
    updated: &[u8],
    to: &Path,
    scrollback: &Path,
) -> Result<()> {
    let old_scrollback = path
        .parent()
        .context("no directory for the state")?
        .parent()
        .context("no state directory")?
        .join("scrollback")
        .join(path.file_stem().unwrap());
    for file in scrollback_files(snap).into_iter().filter(|f| scrollback_name(f)) {
        let source = old_scrollback.join(file);
        let bytes = match paths::read_regular(&source, crate::pane::MAX_SAVED_BYTES as u64) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context("reading saved scrollback before migration"),
        };
        durable_dir(scrollback)?;
        copy_scrollback(&source, &scrollback.join(file), &bytes)?;
    }
    if scrollback.exists() {
        sync_directory(scrollback)?;
    }
    anyhow::ensure!(paths::read_regular(path, u64::MAX)? == data, "state changed during migration; retry");
    durable_dir(to.parent().context("no directory for the state")?)?;
    paths::write_private(to, updated).with_context(|| {
        format!("migrating {}; the original is unchanged; fix the destination and retry", path.display())
    })?;
    sync_directory(to.parent().unwrap())?;
    Ok(())
}

fn migrate(data: &[u8]) -> Result<(SessionSnapshot, Vec<u8>)> {
    #[derive(Deserialize)]
    struct Revision {
        #[serde(default)]
        revision: u32,
    }
    // Read the original bytes first: Value would silently discard duplicate fields.
    let old: SessionSnapshot = serde_json::from_slice(data)?;
    let revision = serde_json::from_slice::<Revision>(data)?.revision;
    anyhow::ensure!(old.version == 1, "unsupported state version {}; use a newer kraal", old.version);
    anyhow::ensure!(revision <= 1, "unsupported state revision {revision}; use a newer kraal");
    validate(&old, data, true)?;
    let mut ids: HashSet<_> = old.workspaces.iter().map(|w| w.id.clone()).collect();
    let mut value: serde_json::Value = serde_json::from_slice(data)?;
    for (ws, saved) in value["workspaces"].as_array_mut().unwrap().iter_mut().zip(&old.workspaces) {
        if saved.id.is_empty() {
            let mut id = crate::accounts::new_uuid();
            while !ids.insert(id.clone()) {
                id = crate::accounts::new_uuid();
            }
            ws["id"] = serde_json::json!(id);
        }
        for tab in ws["tabs"].as_array_mut().unwrap() {
            if revision == 0 && tab["name"].as_str().is_some_and(old_tab_name) {
                tab["name"] = serde_json::json!("");
            }
        }
    }
    value["version"] = serde_json::json!(VERSION);
    value.as_object_mut().unwrap().remove("revision");
    let updated = serde_json::to_vec_pretty(&value)?;
    Ok((parse(&updated)?, updated))
}

fn old_tab_name(name: &str) -> bool {
    name.strip_prefix("tab ").is_some_and(|n| n.parse::<u32>().is_ok())
}

fn copy_scrollback(from: &Path, to: &Path, bytes: &[u8]) -> Result<()> {
    // Restoration dates the saved output by its modification time, not the time of migration.
    let modified = std::fs::metadata(from)?.modified()?;
    paths::write_private(to, bytes)?;
    let file = std::fs::File::open(to)?;
    file.set_modified(modified)?;
    file.sync_all()?;
    Ok(())
}

/// Closed sessions still carry old names until their next restore. Bind them while the
/// accounts file still tells what those names meant; a running server binds its own panes.
pub(crate) fn bind_accounts(accounts: &crate::accounts::Accounts) -> Result<()> {
    for session in paths::saved_sessions().context("reading saved sessions to migrate accounts")? {
        let Some(_lock) = paths::SessionLock::acquire(&session)? else { continue };
        // 0.2.0 does not hold a session lock. Its listening socket still owns this snapshot.
        if !paths::nobody_listens(&paths::socket_path(&session)) {
            continue;
        }
        match inspect(&session) {
            Ok(Some(_)) => {}
            Err(e) if e.is::<std::io::Error>() => return Err(e),
            _ => continue,
        }
        if load(&session)?.is_none() {
            continue;
        }
        let path = paths::session_state_path(&session);
        let source = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let Some(updated) = bind_saved_accounts(&source, accounts)? else { continue };
        paths::write_private(&path, &updated).with_context(|| format!("migrating accounts in {}", path.display()))?;
    }
    Ok(())
}

pub(super) fn bind_saved_accounts(source: &[u8], accounts: &crate::accounts::Accounts) -> Result<Option<Vec<u8>>> {
    // An unreadable session is left for the restore path, which already explains how to recover it.
    let Ok(snapshot) = serde_json::from_slice::<SessionSnapshot>(source) else { return Ok(None) };
    if snapshot.version != VERSION {
        return Ok(None);
    }
    let mut value: serde_json::Value = serde_json::from_slice(source)?;
    fn pane(value: &mut serde_json::Value, accounts: &crate::accounts::Accounts) -> bool {
        let Some(spec) = value.get_mut("spec") else { return false };
        let Ok(mut pane) = serde_json::from_value::<PaneSpec>(spec.clone()) else { return false };
        if pane.account.is_none() || pane.account_id.is_some() {
            return false;
        }
        accounts.bind_pane(&mut pane);
        let Some(spec) = spec.as_object_mut() else { return false };
        spec.insert("account_id".into(), serde_json::json!(pane.account_id));
        spec.insert("account".into(), serde_json::json!(pane.account));
        spec.insert("env".into(), serde_json::json!(pane.env));
        true
    }
    fn node(value: &mut serde_json::Value, accounts: &crate::accounts::Accounts) -> bool {
        if let Some(p) = value.get_mut("Pane") {
            return pane(p, accounts);
        }
        let mut changed = false;
        if let Some(children) =
            value.get_mut("Split").and_then(|s| s.get_mut("children")).and_then(serde_json::Value::as_array_mut)
        {
            for child in children {
                if let Some(child) = child.get_mut(0) {
                    changed |= node(child, accounts);
                }
            }
        }
        changed
    }
    fn workspace(value: &mut serde_json::Value, accounts: &crate::accounts::Accounts) -> bool {
        let mut changed = value.get_mut("layout_undo").is_some_and(|undo| workspace(undo, accounts));
        for tab in value.get_mut("tabs").and_then(serde_json::Value::as_array_mut).into_iter().flatten() {
            if let Some(root) = tab.get_mut("root") {
                changed |= node(root, accounts);
            }
            for floating in tab.get_mut("floating").and_then(serde_json::Value::as_array_mut).into_iter().flatten() {
                if let Some(floating) = floating.get_mut(0) {
                    changed |= pane(floating, accounts);
                }
            }
        }
        changed
    }
    let mut changed = false;
    for saved in value.get_mut("workspaces").and_then(serde_json::Value::as_array_mut).into_iter().flatten() {
        changed |= workspace(saved, accounts);
    }
    for deleted in value.get_mut("deleted_workspaces").and_then(serde_json::Value::as_array_mut).into_iter().flatten() {
        if let Some(saved) = deleted.get_mut("workspace") {
            changed |= workspace(saved, accounts);
        }
    }
    if changed { Ok(Some(serde_json::to_vec_pretty(&value)?)) } else { Ok(None) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_account_names_gain_identity_without_losing_unknown_fields() {
        let pane = serde_json::json!({
            "name": "agent", "agent": "claude", "agent_session": null,
            "future": "kept", "spec": { "account": "claude:default", "future": [1, 2] }
        });
        let source = serde_json::json!({ "version": VERSION, "future": { "spec": { "account": "claude:default" } }, "workspaces": [{
            "name": "w", "definition": null, "cwd": null, "active": 0, "tabs": [{
                "name": "t", "root": { "Pane": pane }, "floating": [], "focus": 0,
                "show_floating": false, "zoomed": false, "sync": false
            }]
        }] });
        let accounts = crate::accounts::Accounts::empty();
        let bytes = bind_saved_accounts(&serde_json::to_vec(&source).unwrap(), &accounts).unwrap().unwrap();
        let mut migrated: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let spec = &mut migrated["workspaces"][0]["tabs"][0]["root"]["Pane"]["spec"];
        assert_eq!(spec["account_id"], "default-claude");
        assert_eq!(spec["account"], "claude:default");
        assert!(bind_saved_accounts(&bytes, &accounts).unwrap().is_none(), "an identity is never rebound");
        spec.as_object_mut().unwrap().remove("account_id");
        spec.as_object_mut().unwrap().remove("env");
        assert_eq!(migrated, source, "unknown fields and all other state survive");
        assert!(bind_saved_accounts(b"{broken", &accounts).unwrap().is_none());
        let mut future = source.clone();
        future["version"] = serde_json::json!(VERSION + 1);
        assert!(bind_saved_accounts(&serde_json::to_vec(&future).unwrap(), &accounts).unwrap().is_none());
        let restored: SessionSnapshot = serde_json::from_slice(&bytes).unwrap();
        let NodeSnap::Pane(pane) = restored.workspaces[0].tabs[0].root.as_ref().unwrap() else { panic!("pane") };
        assert_eq!(pane.spec.account_id.as_deref(), Some("default-claude"));
        let mut deleted = source.clone();
        deleted["deleted_workspaces"] = serde_json::json!([{
            "workspace": source["workspaces"][0], "position": 0, "deleted_at": 1
        }]);
        deleted["workspaces"] = serde_json::json!([]);
        let bytes = bind_saved_accounts(&serde_json::to_vec(&deleted).unwrap(), &accounts).unwrap().unwrap();
        let migrated: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            migrated["deleted_workspaces"][0]["workspace"]["tabs"][0]["root"]["Pane"]["spec"]["account_id"],
            "default-claude"
        );
        assert_eq!(migrated["future"], source["future"]);
    }

    fn legacy_state() -> serde_json::Value {
        serde_json::json!({
            "version": 1, "revision": 1, "future": { "keep": true }, "workspaces": [{
                "name": "same", "definition": "/missing/source.kdl", "definition_source": "// original\nlayout { pane; }",
                "cwd": "/missing/project", "active": 0, "future": [1, 2],
                "tabs": [{ "name": "tab 1", "root": { "Pane": {
                    "name": "agent", "agent": "claude", "agent_session": "conversation",
                    "spec": { "account": "claude:renamed", "account_id": "kept-account",
                        "env": [["CLAUDE_CONFIG_DIR", "/original/home"]] }, "scrollback": "1-2-3.ans"
                } }, "floating": [], "focus": 0, "show_floating": false, "zoomed": false, "sync": false }]
            }]
        })
    }

    fn directory(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kraal-state-{name}-{}", crate::accounts::new_uuid()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn load_from(dir: &Path) -> Result<Option<SessionSnapshot>> {
        read_from(
            &dir.join("sessions-v2/main.json"),
            &dir.join("sessions/main.json"),
            &dir.join("scrollback-v2/main"),
            true,
        )
    }

    fn source_in(dir: &Path) -> (PathBuf, Vec<u8>) {
        let path = dir.join("sessions/main.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let source = serde_json::to_vec(&legacy_state()).unwrap();
        std::fs::write(&path, &source).unwrap();
        (path, source)
    }

    #[test]
    fn inspecting_a_stopped_session_does_not_migrate_or_invent_identities() {
        let session = format!("inspect-{}", crate::accounts::new_uuid());
        let path = paths::legacy_session_state_path(&session);
        let source = serde_json::to_vec(&legacy_state()).unwrap();
        durable_dir(path.parent().unwrap()).unwrap();
        paths::write_private(&path, &source).unwrap();
        let snap = inspect(&session).unwrap().unwrap();
        assert_eq!(snap.version, 1);
        assert!(snap.workspaces[0].id.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), source);
        assert!(!paths::session_state_path(&session).exists());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn migration_keeps_order_sources_collisions_accounts_and_unknown_fields() {
        let mut source = legacy_state();
        let mut other = source["workspaces"][0].clone();
        other["definition"] = serde_json::json!("/another/source.kdl");
        other["restore_error"] = serde_json::json!("missing shell");
        source["workspaces"].as_array_mut().unwrap().push(other);
        let (snap, updated) = migrate(&serde_json::to_vec(&source).unwrap()).unwrap();
        assert_eq!(snap.workspaces.len(), 2);
        assert_ne!(snap.workspaces[0].id, snap.workspaces[1].id);
        assert!(!snap.workspaces[0].id.is_empty());
        let mut value: serde_json::Value = serde_json::from_slice(&updated).unwrap();
        assert_eq!(value["version"], VERSION);
        assert!(value.get("revision").is_none());
        for ws in value["workspaces"].as_array_mut().unwrap() {
            ws.as_object_mut().unwrap().remove("id");
        }
        value["version"] = serde_json::json!(1);
        value["revision"] = serde_json::json!(1);
        assert_eq!(value, source);
        assert_eq!(parse(&updated).unwrap().workspaces[0].id, snap.workspaces[0].id);
    }

    #[test]
    fn legacy_layout_undo_accounts_are_bound_without_changing_other_saved_state() {
        for deleted in [false, true] {
            let dir = directory("undo-accounts");
            let (path, _) = source_in(&dir);
            let mut source = legacy_state();
            let mut undo = source["workspaces"][0].clone();
            let pane = &mut undo["tabs"][0]["root"]["Pane"];
            pane["future"] = serde_json::json!({ "spec": { "account": "claude:default" } });
            pane["spec"] = serde_json::json!({
                "account": "claude:default",
                "env": [],
                "future": ["kept", { "account": "claude:default" }]
            });
            let pane = pane.clone();
            undo["tabs"][0]["root"] = serde_json::json!({ "Split": {
                "axis": "Row", "children": [[{ "Pane": pane }, 1.0]]
            } });
            undo["tabs"][0]["floating"] = serde_json::json!([
                [pane, { "x": 0.1, "y": 0.2, "w": 0.3, "h": 0.4 }]
            ]);
            undo["layout_undo"] = undo.clone();
            source["workspaces"][0]["layout_undo"] = undo;
            if deleted {
                source["workspaces"][0]["id"] = serde_json::json!("deleted-workspace");
                source["deleted_workspaces"] = serde_json::json!([{
                    "workspace": source["workspaces"][0], "position": 0, "deleted_at": 1
                }]);
                source["workspaces"] = serde_json::json!([]);
            }
            let original = serde_json::to_vec_pretty(&source).unwrap();
            std::fs::write(&path, &original).unwrap();
            load_from(&dir).unwrap().unwrap();
            let current = dir.join("sessions-v2/main.json");
            let before = std::fs::read(&current).unwrap();
            let accounts = crate::accounts::Accounts::empty();
            let updated = bind_saved_accounts(&before, &accounts).unwrap().expect("Undo accounts are bound");
            let mut expected: serde_json::Value = serde_json::from_slice(&before).unwrap();
            let workspace = if deleted {
                &mut expected["deleted_workspaces"][0]["workspace"]
            } else {
                &mut expected["workspaces"][0]
            };
            for path in ["/layout_undo", "/layout_undo/layout_undo"] {
                let tab = &mut workspace.pointer_mut(path).unwrap()["tabs"][0];
                for path in ["/root/Split/children/0/0/Pane/spec", "/floating/0/0/spec"] {
                    let spec = tab.pointer_mut(path).unwrap();
                    spec["account_id"] = serde_json::json!("default-claude");
                    spec["env"] = serde_json::json!([
                        ["KRAAL_ACCOUNT_ID", "default-claude"],
                        ["KRAAL_ACCOUNT", "claude:default"]
                    ]);
                }
            }
            assert_eq!(updated, serde_json::to_vec_pretty(&expected).unwrap(), "only account bindings change");
            assert!(bind_saved_accounts(&updated, &accounts).unwrap().is_none(), "identities are never rebound");
            assert_eq!(std::fs::read(&path).unwrap(), original);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn migration_preserves_the_meaning_of_legacy_tab_names() {
        let mut source = legacy_state();
        source.as_object_mut().unwrap().remove("revision");
        let (snap, _) = migrate(&serde_json::to_vec(&source).unwrap()).unwrap();
        assert_eq!(snap.workspaces[0].tabs[0].name, "");
        source["revision"] = serde_json::json!(1);
        let (snap, _) = migrate(&serde_json::to_vec(&source).unwrap()).unwrap();
        assert_eq!(snap.workspaces[0].tabs[0].name, "tab 1");
    }

    #[test]
    fn unreleased_revision_is_refused_without_changing_any_files() {
        let dir = directory("unreleased");
        let (path, _) = source_in(&dir);
        let scrollback = dir.join("scrollback/main/1-2-3.ans");
        std::fs::create_dir_all(scrollback.parent().unwrap()).unwrap();
        std::fs::write(&scrollback, "original text").unwrap();
        let mut source = legacy_state();
        source["revision"] = serde_json::json!(2);
        source["workspaces"][0]["id"] = serde_json::json!("stable");
        source["workspaces"][0]["empty"] = serde_json::json!(true);
        source["workspaces"][0]["closed"] = serde_json::json!(true);
        let mut empty = source.clone();
        empty["workspaces"] = serde_json::json!([]);
        for source in [source, empty] {
            let bytes = serde_json::to_vec(&source).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            for convert in [false, true] {
                let error =
                    read_from(&dir.join("sessions-v2/main.json"), &path, &dir.join("scrollback-v2/main"), convert)
                        .unwrap_err();
                assert_eq!(error.to_string(), "unsupported state revision 2; use a newer kraal");
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                assert_eq!(std::fs::read_to_string(&scrollback).unwrap(), "original text");
                assert_eq!(std::fs::read_dir(dir.join("sessions")).unwrap().count(), 1);
                assert!(!dir.join("sessions-v2").exists());
                assert!(!dir.join("scrollback-v2").exists());
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migration_is_private_complete_once_and_independent_of_older_writers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = directory("migration");
        let (path, source) = source_in(&dir);
        let scrollback = dir.join("scrollback/main");
        std::fs::create_dir_all(&scrollback).unwrap();
        std::fs::write(scrollback.join("1-2-3.ans"), "original text").unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::new(1_600_000_000, 123_456_789);
        std::fs::File::open(scrollback.join("1-2-3.ans")).unwrap().set_modified(modified).unwrap();
        let loaded = load_from(&dir).unwrap().unwrap();
        for file in [scrollback.join("1-2-3.ans"), dir.join("scrollback-v2/main/1-2-3.ans")] {
            assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), modified, "{}", file.display());
        }
        assert_eq!(std::fs::read(&path).unwrap(), source);
        assert_eq!(std::fs::read_to_string(scrollback.join("1-2-3.ans")).unwrap(), "original text");
        let current = dir.join("sessions-v2/main.json");
        assert_eq!(std::fs::metadata(&current).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(current.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        let copied = dir.join("scrollback-v2/main/1-2-3.ans");
        assert_eq!(std::fs::metadata(&copied).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(copied.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        let bytes = std::fs::read(&current).unwrap();
        std::fs::write(&path, b"old writer changed this").unwrap();
        std::fs::remove_dir_all(scrollback).unwrap();
        assert_eq!(load_from(&dir).unwrap().unwrap().workspaces[0].id, loaded.workspaces[0].id);
        assert_eq!(std::fs::read_dir(dir.join("sessions")).unwrap().count(), 1);
        assert_eq!(std::fs::read(&current).unwrap(), bytes);
        assert_eq!(std::fs::read_to_string(dir.join("scrollback-v2/main/1-2-3.ans")).unwrap(), "original text");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn forgetting_current_or_unconverted_state_preserves_legacy_files_and_prevents_reimport() {
        for converted in [false, true] {
            let dir = directory("forget");
            let (legacy, original) = source_in(&dir);
            let scrollback = dir.join("scrollback/main/1-2-3.ans");
            std::fs::create_dir_all(scrollback.parent().unwrap()).unwrap();
            std::fs::write(&scrollback, b"original output\0\xff").unwrap();
            let current = dir.join("sessions-v2/main.json");
            if converted {
                load_from(&dir).unwrap().unwrap();
            }
            assert!(forget_from(&current, &legacy).unwrap());
            assert!(!current.exists());
            assert!(current.with_extension("forgotten").is_file());
            for convert in [false, true] {
                assert!(read_from(&current, &legacy, &dir.join("scrollback-v2/main"), convert).unwrap().is_none());
            }
            assert!(!forget_from(&current, &legacy).unwrap());
            assert_eq!(std::fs::read(&legacy).unwrap(), original);
            assert_eq!(std::fs::read(&scrollback).unwrap(), b"original output\0\xff");
            assert_eq!(std::fs::read_dir(dir.join("sessions")).unwrap().count(), 1);
            // A later save takes precedence over the marker, including after another Forget.
            let (_, updated) = migrate(&original).unwrap();
            paths::write_private(&current, &updated).unwrap();
            assert!(load_from(&dir).unwrap().is_some());
            assert!(forget_from(&current, &legacy).unwrap());
            assert!(load_from(&dir).unwrap().is_none());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn forgetting_a_dangling_legacy_symlink_prevents_reimport_when_its_target_returns() {
        let dir = directory("forget-dangling-link");
        let (legacy, original) = source_in(&dir);
        let target = dir.join("missing.json");
        std::fs::remove_file(&legacy).unwrap();
        std::os::unix::fs::symlink(&target, &legacy).unwrap();
        let current = dir.join("sessions-v2/main.json");
        assert!(load_from(&dir).is_err());
        assert!(forget_from(&current, &legacy).unwrap());
        assert!(current.with_extension("forgotten").is_file());
        assert!(!target.exists());
        for returned in [false, true] {
            if returned {
                std::fs::write(&target, &original).unwrap();
            }
            for convert in [false, true] {
                assert!(read_from(&current, &legacy, &dir.join("scrollback-v2/main"), convert).unwrap().is_none());
            }
            assert_eq!(std::fs::read_link(&legacy).unwrap(), target);
            assert!(!current.exists());
            assert!(!dir.join("scrollback-v2").exists());
            assert!(!forget_from(&current, &legacy).unwrap());
        }
        assert_eq!(std::fs::read(&target).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn forgetting_without_legacy_state_removes_current_state_without_a_marker() {
        let dir = directory("forget-current");
        let legacy = dir.join("sessions/main.json");
        let current = dir.join("sessions-v2/main.json");
        assert!(!forget_from(&current, &legacy).unwrap());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::create_dir(current.parent().unwrap()).unwrap();
        let (_, updated) = migrate(&serde_json::to_vec(&legacy_state()).unwrap()).unwrap();
        paths::write_private(&current, &updated).unwrap();
        assert!(forget_from(&current, &legacy).unwrap());
        assert!(!current.exists());
        assert!(!current.with_extension("forgotten").exists());
        assert!(!forget_from(&current, &legacy).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_forget_marker_keeps_current_and_legacy_state_for_retry() {
        let dir = directory("forget-failure");
        let (legacy, original) = source_in(&dir);
        load_from(&dir).unwrap().unwrap();
        let current = dir.join("sessions-v2/main.json");
        let saved = std::fs::read(&current).unwrap();
        let parent = current.parent().unwrap();
        let aside = dir.join("current");
        std::fs::rename(parent, &aside).unwrap();
        std::fs::write(parent, "not a directory").unwrap();
        assert!(forget_from(&current, &legacy).is_err());
        assert_eq!(std::fs::read(&legacy).unwrap(), original);
        assert_eq!(std::fs::read(aside.join("main.json")).unwrap(), saved);
        std::fs::remove_file(parent).unwrap();
        std::fs::rename(aside, parent).unwrap();
        assert!(forget_from(&current, &legacy).unwrap());
        assert!(load_from(&dir).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migration_needs_no_writable_legacy_directory_and_retries_a_failed_destination() {
        use std::os::unix::fs::PermissionsExt;
        let dir = directory("read-only-legacy");
        let (legacy, original) = source_in(&dir);
        let old_scrollback = dir.join("scrollback/main");
        std::fs::create_dir_all(&old_scrollback).unwrap();
        let output = old_scrollback.join("1-2-3.ans");
        std::fs::write(&output, b"original output").unwrap();
        for file in [&legacy, &output] {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o400)).unwrap();
        }
        for dir in [legacy.parent().unwrap(), old_scrollback.as_path()] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        let destination = dir.join("scrollback-v2/main/1-2-3.ans");
        std::fs::create_dir_all(&destination).unwrap();
        assert!(load_from(&dir).is_err());
        assert!(!dir.join("sessions-v2/main.json").exists());
        assert_eq!(std::fs::read(&legacy).unwrap(), original);
        assert_eq!(std::fs::read(&output).unwrap(), b"original output");
        std::fs::remove_dir(&destination).unwrap();
        assert!(load_from(&dir).unwrap().is_some());
        assert_eq!(std::fs::read(&destination).unwrap(), b"original output");
        assert_eq!(std::fs::read(&legacy).unwrap(), original);
        assert_eq!(std::fs::read(&output).unwrap(), b"original output");
        assert_eq!(std::fs::read_dir(legacy.parent().unwrap()).unwrap().count(), 1);
        for dir in [legacy.parent().unwrap(), old_scrollback.as_path()] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_new_state_never_falls_back_to_the_upgrade_copy() {
        let dir = directory("no-fallback");
        let (legacy, original) = source_in(&dir);
        load_from(&dir).unwrap();
        let current = dir.join("sessions-v2/main.json");
        for bytes in ["{", r#"{"version":3,"future_layout":[]}"#] {
            std::fs::write(&current, bytes).unwrap();
            assert!(load_from(&dir).is_err());
            assert_eq!(std::fs::read_to_string(&current).unwrap(), bytes);
            assert_eq!(std::fs::read(&legacy).unwrap(), original);
            assert_eq!(std::fs::read_dir(dir.join("sessions")).unwrap().count(), 1);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_scrollback_leaves_the_state_unchanged_and_can_be_retried() {
        let dir = directory("unreadable-scrollback");
        let (path, source) = source_in(&dir);
        let scrollback = dir.join("scrollback/main/1-2-3.ans");
        std::fs::create_dir_all(&scrollback).unwrap();
        assert!(load_from(&dir).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), source);
        assert!(!dir.join("sessions-v2/main.json").exists());
        std::fs::remove_dir(scrollback).unwrap();
        assert!(load_from(&dir).unwrap().is_some(), "missing scrollback does not prevent restoring the layout");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_or_newer_legacy_state_stays_untouched() {
        let dir = directory("invalid");
        let (path, _) = source_in(&dir);
        let mut cases = vec![b"{".to_vec()];
        for (field, value) in [("version", 2), ("revision", 3), ("revision", 2)] {
            let mut source = legacy_state();
            source[field] = serde_json::json!(value);
            cases.push(serde_json::to_vec(&source).unwrap());
        }
        for id in ["bad/id", "duplicate"] {
            let mut source = legacy_state();
            source["workspaces"][0]["id"] = serde_json::json!(id);
            let ws = source["workspaces"][0].clone();
            source["workspaces"].as_array_mut().unwrap().push(ws);
            cases.push(serde_json::to_vec(&source).unwrap());
        }
        for source in cases {
            std::fs::write(&path, &source).unwrap();
            assert!(load_from(&dir).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), source);
            assert_eq!(std::fs::read_dir(dir.join("sessions")).unwrap().count(), 1);
        }
        std::fs::remove_file(&path).unwrap();
        assert!(load_from(&dir).unwrap().is_none());
        std::os::unix::fs::symlink(dir.join("missing"), &path).unwrap();
        assert!(load_from(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
