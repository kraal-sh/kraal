//! Migration from pre-0.4 state. Remove this module in 0.5.0.

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
    let backup = backup_state(path, data, snap, &old_scrollback)?;
    for file in scrollback_files(snap).into_iter().filter(|f| scrollback_name(f)) {
        let source = backup.join("scrollback").join(file);
        let Some(bytes) = read_state(&source)? else { continue };
        durable_dir(scrollback)?;
        copy_scrollback(&source, &scrollback.join(file), &bytes)?;
    }
    if scrollback.exists() {
        sync_directory(scrollback)?;
    }
    anyhow::ensure!(paths::read_regular(path, u64::MAX)? == data, "state changed during migration; retry");
    durable_dir(to.parent().context("no directory for the state")?)?;
    paths::write_private(to, updated)
        .with_context(|| format!("migrating {}; the original is also in {}", path.display(), backup.display()))?;
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

fn backup_state(path: &Path, data: &[u8], snap: &SessionSnapshot, scrollback: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let backup = path.with_extension(format!("json.before-0.4-{}", crate::accounts::new_uuid()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&backup)
        .with_context(|| format!("creating migration backup {}", backup.display()))?;
    paths::sync_dir(path.parent().context("no directory for the state")?);
    // The live scrollback is overwritten and pruned later, so the rollback copy must own its bytes.
    for file in scrollback_files(snap).into_iter().filter(|f| scrollback_name(f)) {
        let bytes = match paths::read_regular(&scrollback.join(file), crate::pane::MAX_SAVED_BYTES as u64) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context("backing up saved scrollback before migration"),
        };
        let dir = backup.join("scrollback");
        durable_dir(&dir)?;
        copy_scrollback(&scrollback.join(file), &dir.join(file), &bytes)?;
        paths::sync_dir(&dir);
    }
    // Written last: a backup without state.json was interrupted and cannot be used for rollback.
    paths::write_private(&backup.join("state.json"), data)?;
    paths::sync_dir(&backup);
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn backups(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir.join("sessions"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("main.json.before-0.4-"))
            .collect()
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
        assert!(
            !std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(&format!("{session}.json.before-")))
        );
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
            assert_eq!(std::fs::read(backups(&dir)[0].join("state.json")).unwrap(), original);
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
                assert!(backups(&dir).is_empty());
                assert!(!dir.join("sessions-v2").exists());
                assert!(!dir.join("scrollback-v2").exists());
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migration_is_private_complete_once_and_independent_of_older_writers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = directory("backup");
        let (path, source) = source_in(&dir);
        let scrollback = dir.join("scrollback/main");
        std::fs::create_dir_all(&scrollback).unwrap();
        std::fs::write(scrollback.join("1-2-3.ans"), "original text").unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::new(1_600_000_000, 123_456_789);
        std::fs::File::open(scrollback.join("1-2-3.ans")).unwrap().set_modified(modified).unwrap();
        let loaded = load_from(&dir).unwrap().unwrap();
        let backup = backups(&dir).pop().unwrap();
        for file in [
            scrollback.join("1-2-3.ans"),
            backup.join("scrollback/1-2-3.ans"),
            dir.join("scrollback-v2/main/1-2-3.ans"),
        ] {
            assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), modified, "{}", file.display());
        }
        assert_eq!(std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(backup.join("state.json")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(backup.join("state.json")).unwrap(), source);
        assert_eq!(std::fs::read(&path).unwrap(), source);
        assert_eq!(std::fs::read_to_string(scrollback.join("1-2-3.ans")).unwrap(), "original text");
        let current = dir.join("sessions-v2/main.json");
        assert_eq!(std::fs::metadata(&current).unwrap().permissions().mode() & 0o777, 0o600);
        let bytes = std::fs::read(&current).unwrap();
        std::fs::write(&path, b"old writer changed this").unwrap();
        std::fs::remove_dir_all(scrollback).unwrap();
        assert_eq!(load_from(&dir).unwrap().unwrap().workspaces[0].id, loaded.workspaces[0].id);
        assert_eq!(backups(&dir).len(), 1);
        assert_eq!(std::fs::read(&current).unwrap(), bytes);
        assert_eq!(std::fs::read_to_string(dir.join("scrollback-v2/main/1-2-3.ans")).unwrap(), "original text");
        assert_eq!(std::fs::read_to_string(backup.join("scrollback/1-2-3.ans")).unwrap(), "original text");
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
            assert_eq!(backups(&dir).len(), 1);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_backup_leaves_the_state_unchanged_and_can_be_retried() {
        let dir = directory("failed-backup");
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
            assert!(backups(&dir).is_empty());
        }
        std::fs::remove_file(&path).unwrap();
        assert!(load_from(&dir).unwrap().is_none());
        std::os::unix::fs::symlink(dir.join("missing"), &path).unwrap();
        assert!(load_from(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
