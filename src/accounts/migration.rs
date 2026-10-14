//! Temporary 0.4 migration, including the files that must be bound before accounts change.
//!
//! In 0.5.0 remove this module and its hooks (legacy ID synthesis, empty TOML sections, backups,
//! usage copying and fallback reads, and `before_account_change`), `persist::migration`
//! (legacy session conversion and saved reference rewriting), `workspace::migration`
//! (external KDL binding), and their migration tests. Keep ordinary account resolution,
//! retained references and homes, `WorkspaceSnap::bind_accounts`, and
//! `workspace::bind_source_accounts`: current operations still use them.

use super::*;

/// Bind names while they still identify the accounts from before the change. A failure
/// must reach the caller before it publishes accounts.toml.
pub(super) fn before_account_change(previous: &Accounts) -> Result<()> {
    if previous.path == Accounts::path() {
        for a in previous.known() {
            migrate_usage(&usage_dir(), &a)?;
        }
        crate::persist::migration::bind_accounts(previous).context(
            "could not migrate saved account references; accounts.toml is unchanged; check the saved session and retry",
        )?;
        for dir in crate::config::Config::load()?.workspace_dirs() {
            for (file, definition) in crate::workspace::load_dir(&dir) {
                if definition.is_ok() {
                    crate::workspace::migration::bind_accounts(&file, previous).with_context(|| {
                        format!(
                            "could not migrate {}; accounts.toml is unchanged; check this file and retry",
                            file.display()
                        )
                    })?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn identify(file: &mut File) {
    for account in file.accounts.iter_mut().chain(&mut file.forgotten) {
        identify_account(account);
        for reference in &mut account.references {
            identify_account(reference);
        }
    }
}

pub(super) fn identify_account(account: &mut Account) {
    if account.id.is_empty() {
        account.id = account.legacy_id();
    }
}

impl Account {
    /// Readers of an old file must agree before any of them rewrites it.
    pub(super) fn legacy_id(&self) -> String {
        format!("legacy-{:016x}", crate::text::fnv64(&format!("{}\0{}\0{}", self.agent, self.name, self.home)))
    }
}

/// Remove the empty sections older kraal versions wrote, using their source spans so that
/// comments and unrelated formatting stay intact.
pub(super) fn without_empty_sections(mut text: String) -> Result<String> {
    let doc = toml_edit::Document::parse(text.as_str())?;
    let mut remove = Vec::new();
    if let Some(array) = doc.get("rule").and_then(toml_edit::Item::as_array).filter(|array| array.is_empty())
        && let (Some(key), Some(value)) = (doc.key("rule").and_then(toml_edit::Key::span), array.span())
    {
        remove.push(key.start..value.start + 1);
        remove.push(value.end - 1..value.end);
    }
    if let Some(table) = doc.get("claude").and_then(toml_edit::Item::as_table).filter(|table| table.is_empty())
        && let Some(span) = table.span()
    {
        remove.push(span);
    }
    remove.sort_by_key(|span| span.start);
    for span in remove.into_iter().rev() {
        let start = text[..span.start].rfind('\n').map_or(0, |i| i + 1);
        let end = text[span.end..].find('\n').map_or(text.len(), |i| span.end + i + 1);
        // Drop a line only when removing its syntax leaves no comment on it.
        if text[start..span.start].trim().is_empty() && text[span.end..end].trim().is_empty() {
            let mut end = end;
            if text[..start].lines().next_back().is_some_and(|line| line.trim().is_empty()) {
                // Removing a section joins its surrounding blank lines into one separator.
                while let Some(line) = text[end..].split_inclusive('\n').next().filter(|line| line.trim().is_empty()) {
                    end += line.len();
                }
            }
            text.replace_range(start..end, "");
        } else {
            text.replace_range(span, "");
        }
    }
    Ok(text)
}

pub(super) fn backup(accounts: &Accounts, text: &str) -> Result<()> {
    if let Some(source) = &accounts.source {
        let old: File = toml::from_str(source)?;
        if old.rules != accounts.rules || old.expanded_accounts() != accounts.accounts || source != text {
            let backup = accounts.path.with_extension("toml.kraal-bak");
            if !backup.exists() {
                paths::write_atomic(&backup, source.as_bytes()).context("backing up accounts before migration")?;
            }
        }
    }
    Ok(())
}

pub(super) fn legacy_usage_name(a: &Account) -> Option<&str> {
    if a.id == format!("default-{}", a.agent) {
        Some("default")
    } else if a.id == a.legacy_id() {
        Some(&a.name)
    } else {
        None
    }
}

/// Copy legacy usage once; new accounts never inherit a previous occupant's cache.
pub(super) fn migrate_usage(dir: &Path, a: &Account) -> Result<()> {
    let to = dir.join("by-id").join(usage_file(&a.agent, &a.id));
    if let Some(name) = legacy_usage_name(a)
        && !to.exists()
    {
        let from = dir.join(usage_file(&a.agent, name));
        match std::fs::read(&from) {
            Ok(bytes) => {
                paths::ensure_private_dir(&dir.join("by-id"))?;
                paths::write_private(&to, &bytes)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("reading legacy account usage"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::tests::{acct, changed_text};

    #[test]
    fn legacy_accounts_migrate_on_rewrite_with_a_backup_and_stable_references() {
        let dir = std::env::temp_dir().join(format!("kraal-account-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        let source = "# Mine.\n[[account]]\nagent = 'claude'\nname = 'work' # label\nhome = '/work'\ncolor = 'blue'\n\n[[rule]]\nworkspace = 'w'\naccounts = [\n  'claude:work', # preferred\n]\n";
        std::fs::write(&path, source).unwrap();
        let before = Accounts::load_from(&path).unwrap();
        let account = before.get("claude", "work").unwrap();
        assert_eq!(Accounts::load_from(&path).unwrap().get("claude", "work").unwrap().id, account.id);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source, "reading does not rewrite");
        Accounts::update_at(&path, |_| Ok(())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source, "a no-op does not rewrite");
        Accounts::update_at(&path, |all| all.rename("claude", "work", "job")).unwrap();
        let migrated = std::fs::read_to_string(&path).unwrap();
        let expected = source
            .replace("name = 'work'", "name = \"job\"")
            .replace("color = 'blue'\n", &format!("color = 'blue'\nid = {:?}\n", account.id))
            .replace("'claude:work'", "\"job\"");
        let expected = format!("{expected}account_ids = [{:?}]\n", account.key());
        assert_eq!(migrated, expected);
        assert_eq!(std::fs::read_to_string(path.with_extension("toml.kraal-bak")).unwrap(), source);
        let after = Accounts::load_from(&path).unwrap();
        assert_eq!(after.get("claude", "job").unwrap().id, account.id);
        assert_eq!(after.get("claude", &format!("@{}", account.id)).unwrap().name, "job");
        assert_eq!(after.get("claude", &account.key()).unwrap().name, "job");
        assert_eq!(after.get("claude", "claude:job").unwrap().id, account.id);
        assert_eq!(after.choose("claude", Some("w"), None, &UsageMap::default(), 0).unwrap().id, account.id);
        Accounts::update_at(&path, |all| all.rename("claude", "job", "team")).unwrap();
        assert_eq!(std::fs::read_to_string(path.with_extension("toml.kraal-bak")).unwrap(), source);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_usage_moves_once_and_a_reused_label_gets_no_usage() {
        let dir = std::env::temp_dir().join(format!("kraal-usage-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = acct("claude", "work", "/work");
        std::fs::write(dir.join("claude-work.json"), "old usage").unwrap();
        migrate_usage(&dir, &old).unwrap();
        let file = dir.join("by-id").join(usage_file("claude", &old.id));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old usage");
        std::fs::write(&file, "new usage").unwrap();
        migrate_usage(&dir, &old).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new usage");
        let replacement = Account { id: new_uuid(), ..old.clone() };
        migrate_usage(&dir, &replacement).unwrap();
        assert!(!dir.join("by-id").join(usage_file("claude", &replacement.id)).exists());
        let renamed = Account { name: "another".into(), ..old };
        std::fs::remove_file(&file).unwrap();
        std::fs::write(dir.join("claude-another.json"), "someone else's usage").unwrap();
        migrate_usage(&dir, &renamed).unwrap();
        assert!(!file.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn identity_usage_cannot_collide_with_legacy_account_names() {
        let dir = std::env::temp_dir().join(format!("kraal-usage-collision-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let default = Accounts::empty().get("claude", "default").unwrap();
        let work = acct("claude", "work", "/work");
        for account in [default, work] {
            let namesake = acct("claude", &account.id, "/other");
            let legacy = dir.join(usage_file("claude", &account.name));
            let collision = dir.join(usage_file("claude", &namesake.name));
            std::fs::write(&legacy, "own usage").unwrap();
            std::fs::write(&collision, "namesake usage").unwrap();
            migrate_usage(&dir, &account).unwrap();
            migrate_usage(&dir, &namesake).unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.join("by-id").join(usage_file("claude", &account.id))).unwrap(),
                "own usage"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("by-id").join(usage_file("claude", &namesake.id))).unwrap(),
                "namesake usage"
            );
            assert_eq!(
                std::fs::read_to_string(collision).unwrap(),
                "namesake usage",
                "legacy servers still own their caches"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removed_legacy_sections_leave_one_blank_line_without_reformatting_other_content() {
        for newline in ["\n", "\r\n"] {
            let source = "# Accounts.\n\nrule = []\n\n[limits]\nthreshold = 90\nmode = 'notify'\n\n[claude]\n\n# My settings.\n\n\n[custom]\ncolor = 'blue'\n".replace('\n', newline);
            let expected = "# Accounts.\n\n[limits]\nthreshold = 90\nmode = 'notify'\n\n# My settings.\n\n\n[custom]\ncolor = 'blue'\n".replace('\n', newline);
            let written = changed_text("empty-spacing", &source, |all| all.save());
            assert_eq!(written, expected);
            assert_eq!(changed_text("empty-spacing-again", &written, |all| all.save()), written);
        }
    }

    #[test]
    fn empty_sections_go_when_the_file_is_rewritten_and_their_comments_stay() {
        let old = "# My accounts.\nrule = []\n\n[claude]\n\n[limits]\nthreshold = 85 # mine\n";
        let text = changed_text("empty", old, |all| {
            all.limits.threshold = 70;
            Ok(())
        });
        assert_eq!(text, "# My accounts.\n\n[limits]\nthreshold = 70 # mine\n");
        assert_eq!(changed_text("empty-again", &text, |all| all.save()), text);

        let old = "# My accounts.\n'rule' = [ # contexts\n    # No rules yet.\n] # later\ncolor = 'blue'\n\n\
                   [ 'claude' ] # my status line\n# Kept for later.\n\n[limits]\nthreshold = 85 # mine\n";
        let text = changed_text("empty-comments", old, |all| all.save());
        assert_eq!(text, old.replace("'rule' = [", "").replace("] # later", " # later").replace("[ 'claude' ]", ""));
        let windows = old.replace('\n', "\r\n");
        assert_eq!(changed_text("empty-crlf", &windows, |all| all.save()), text.replace('\n', "\r\n"));
        assert_eq!(without_empty_sections("rule = []\n[claude]".into()).unwrap(), "");
    }
}
