//! Temporary 0.4 KDL migration. See `accounts::migration` for the 0.5.0 removal boundary.

use super::*;

/// Bind old labels before a rename or removal, keeping the source formatting.
pub(crate) fn bind_accounts(path: &Path, accounts: &crate::accounts::Accounts) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
