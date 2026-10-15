//! Shared workspace rows, with presentation and source collection left to their consumers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::WorkspaceDef;
use crate::persist::{DeletedWorkspace, WorkspaceKey, WorkspaceSnap};
use crate::protocol::WorkspaceInfo;

pub struct Kept<'a, T> {
    pub workspace: T,
    pub id: Option<&'a str>,
    pub definition: Option<&'a Path>,
}

pub enum SourceData<'a> {
    Loaded(&'a WorkspaceDef),
    Invalid(&'a anyhow::Error),
    Cached(&'a str),
}

pub struct Source<'a> {
    pub path: &'a Path,
    pub data: SourceData<'a>,
    pub collision: bool,
}

impl<'a> Source<'a> {
    pub fn loaded((path, result): &'a (PathBuf, anyhow::Result<WorkspaceDef>)) -> Self {
        let data = match result {
            Ok(def) => SourceData::Loaded(def),
            Err(error) => SourceData::Invalid(error),
        };
        Self { path, data, collision: false }
    }

    pub fn cached((path, name): &'a (PathBuf, String)) -> Self {
        Self { path, data: SourceData::Cached(name), collision: false }
    }

    pub fn name(&self) -> Option<&'a str> {
        match self.data {
            SourceData::Loaded(def) => Some(&def.name),
            SourceData::Cached(name) => Some(name),
            SourceData::Invalid(_) => None,
        }
    }

    pub fn info(&self) -> Option<WorkspaceInfo> {
        let SourceData::Loaded(def) = self.data else { return None };
        Some(WorkspaceInfo {
            id: None,
            name: def.name.clone(),
            open: false,
            definition: Some(self.path.to_path_buf()),
            tabs: def.tabs.len(),
            panes: def.panes().len(),
            working: 0,
            blocked: 0,
            done: 0,
        })
    }
}

pub enum Entry<'a, T> {
    Kept(Kept<'a, T>),
    Source(Source<'a>),
}

impl<T> Entry<'_, T> {
    pub fn position(&self, order: &[WorkspaceKey]) -> usize {
        match self {
            Self::Kept(w) => WorkspaceKey::position(order, w.id, w.definition),
            Self::Source(s) => WorkspaceKey::position(order, None, Some(s.path)),
        }
    }
}

pub fn entries<'a, T>(
    kept: impl IntoIterator<Item = Kept<'a, T>>,
    sources: impl IntoIterator<Item = Source<'a>>,
    available: impl Fn(&Path) -> bool,
) -> Vec<Entry<'a, T>> {
    let mut sources: Vec<_> = sources.into_iter().collect();
    let mut names = HashSet::new();
    let mut collisions = HashSet::new();
    // An associated source still disambiguates another file with the same name.
    for name in sources.iter().filter_map(Source::name) {
        if !names.insert(name) {
            collisions.insert(name);
        }
    }
    for source in &mut sources {
        source.collision = source.name().is_some_and(|name| collisions.contains(name));
    }
    let mut entries: Vec<_> = kept.into_iter().map(Entry::Kept).collect();
    entries.extend(sources.into_iter().filter(|s| available(s.path)).map(Entry::Source));
    entries
}

pub fn saved<'a>(
    workspaces: &'a [WorkspaceSnap],
    sources: &'a [(PathBuf, anyhow::Result<WorkspaceDef>)],
    removed: &[PathBuf],
) -> Vec<Entry<'a, &'a WorkspaceSnap>> {
    entries(
        workspaces.iter().map(|w| Kept {
            workspace: w,
            id: (!w.id.is_empty()).then_some(w.id.as_str()),
            definition: w.definition.as_deref(),
        }),
        sources.iter().map(Source::loaded),
        |path| {
            !removed.iter().any(|p| crate::paths::same_file(p, path))
                && !workspaces.iter().any(|w| w.definition.as_deref().is_some_and(|p| crate::paths::same_file(p, path)))
        },
    )
}

pub fn saved_info(w: &WorkspaceSnap) -> WorkspaceInfo {
    let (tabs, panes) = w.counts();
    WorkspaceInfo {
        id: (!w.id.is_empty()).then(|| w.id.clone()),
        name: w.name.clone(),
        open: false,
        definition: w.definition.clone(),
        tabs,
        panes,
        working: 0,
        blocked: 0,
        done: 0,
    }
}

pub fn deleted_infos(deleted: &[DeletedWorkspace]) -> Vec<WorkspaceInfo> {
    deleted
        .iter()
        .rev()
        .map(|d| {
            let mut info = saved_info(&d.workspace);
            info.id = Some(d.workspace.id.clone());
            info
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn snapshot(id: &str, name: &str, definition: Option<&Path>) -> WorkspaceSnap {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "definition": definition, "cwd": null,
            "active": 0, "tabs": [], "empty": true
        }))
        .unwrap()
    }

    fn infos(entries: Vec<Entry<'_, &WorkspaceSnap>>) -> serde_json::Value {
        serde_json::to_value(
            entries
                .into_iter()
                .filter_map(|entry| match entry {
                    Entry::Kept(w) => Some(saved_info(w.workspace)),
                    Entry::Source(s) => s.info(),
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn associated_files_still_disambiguate_sources_and_invalid_files_stay_available() {
        let dir = TempDir::new("catalog-collisions");
        let first = dir.mkdir("first").join("same.kdl");
        let second = dir.mkdir("second").join("same.kdl");
        let broken = dir.write("broken.kdl", "layout {");
        for path in [&first, &second] {
            std::fs::write(path, "layout { pane; }").unwrap();
        }
        let sources: Vec<_> =
            [&first, &second, &broken].into_iter().map(|p| (p.clone(), crate::workspace::load_file(p))).collect();
        let kept = [snapshot("kept", "personal", Some(&first))];
        let catalog = saved(&kept, &sources, &[]);
        assert_eq!(catalog.len(), 3);
        let Entry::Source(source) = &catalog[1] else { panic!() };
        assert_eq!(source.path, second);
        assert!(source.collision);
        assert_eq!(source.info().unwrap().panes, 1);
        let Entry::Source(source) = &catalog[2] else { panic!() };
        assert!(matches!(source.data, SourceData::Invalid(_)));
        assert!(source.info().is_none());
        assert_eq!(infos(catalog).as_array().unwrap().len(), 2);
    }

    #[test]
    fn saved_associations_and_removals_match_aliases_and_missing_paths() {
        let dir = TempDir::new("catalog-associations");
        let linked = dir.write("linked.kdl", "layout { pane; }");
        let removed = dir.write("removed.kdl", "layout { pane; }");
        let alias = dir.0.join("alias.kdl");
        let removed_alias = dir.0.join("removed-alias.kdl");
        std::os::unix::fs::symlink(&linked, &alias).unwrap();
        std::fs::hard_link(&removed, &removed_alias).unwrap();
        let missing = dir.0.join("missing.kdl");
        let kept = [snapshot("linked", "personal", Some(&linked)), snapshot("missing", "missing", Some(&missing))];
        let sources: Vec<_> = [&alias, &removed_alias, &missing]
            .into_iter()
            .map(|p| (p.clone(), crate::workspace::load_file(p)))
            .collect();
        assert_eq!(saved(&kept, &sources, &[removed]).len(), 2);
        std::fs::remove_file(linked).unwrap();
        assert_eq!(saved(&kept, &sources, &[removed_alias]).len(), 2);
    }

    #[test]
    fn saved_order_keeps_identity_slots_duplicates_and_unavailable_layout_counts() {
        let path = PathBuf::from("/missing/source.kdl");
        let mut unavailable = snapshot("identity", "same", Some(&path));
        unavailable.restore_error = Some("shell missing".into());
        unavailable.unopened = true;
        unavailable.definition_source = Some("layout { pane; pane; }".into());
        let no_id = snapshot("", "same", None);
        let kept = [unavailable, no_id];
        let sources: Vec<(PathBuf, anyhow::Result<WorkspaceDef>)> = vec![
            ("/missing/duplicate.kdl".into(), crate::workspace::parse_workspace("same", "layout { pane; }")),
            ("/missing/duplicate.kdl".into(), crate::workspace::parse_workspace("same", "layout { pane; }")),
        ];
        let order = [
            WorkspaceKey::Source(path),
            WorkspaceKey::Source(sources[0].0.clone()),
            WorkspaceKey::Id("identity".into()),
        ];
        let mut catalog = saved(&kept, &sources, &[]);
        catalog.sort_by_key(|entry| entry.position(&order));
        let list = infos(catalog);
        assert_eq!(list.as_array().unwrap().len(), 4);
        assert_eq!(list[0], list[1]);
        assert!(list[0].get("id").is_none());
        assert_eq!(list[2]["id"], "identity");
        assert_eq!(list[2]["tabs"], 1);
        assert_eq!(list[2]["panes"], 2);
        assert_eq!(list[2]["open"], false);
        assert_eq!(list[2]["working"], 0);
        assert!(list[3].get("id").is_none());
    }

    #[test]
    fn deleted_entries_are_newest_first_and_always_keep_their_ids() {
        let deleted: Vec<_> = ["first", ""]
            .into_iter()
            .enumerate()
            .map(|(position, id)| DeletedWorkspace {
                workspace: snapshot(id, "same", None),
                position,
                order_position: Some(position),
                deleted_at: 1,
            })
            .collect();
        let list = deleted_infos(&deleted);
        assert_eq!(list.iter().map(|w| w.id.as_deref()).collect::<Vec<_>>(), [Some(""), Some("first")]);
        assert!(list.iter().all(|w| !w.open && w.tabs == 0 && w.panes == 0 && w.working + w.blocked + w.done == 0));
    }
}
