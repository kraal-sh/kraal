//! Writing back a TOML file that kraal and its user both write (servers.toml, accounts.toml):
//! what kraal changed is edited into the text as it is. The user's comments, order and blank
//! lines stay, and so do the keys this kraal does not read: another version's, which a machine
//! running that version shares the file with.

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde::de::DeserializeOwned;
use toml_edit::visit_mut::{self, VisitMut};
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

/// `text`, which kraal read as a `T`, once that `T` has become `now`: only what differs between
/// the two is edited in the text.
pub fn rewrite<T: Serialize + DeserializeOwned>(text: &str, now: &T) -> Result<String> {
    let read: T = toml::from_str(text).context("reading the file again")?;
    let (before, after) = (toml::Table::try_from(&read)?, toml::Table::try_from(now)?);
    if before == after {
        return Ok(text.to_string());
    }
    let written = toml::to_string_pretty(now)?;
    let mut doc: DocumentMut = text.parse().context("reading the file to edit it")?;
    if doc.is_empty() {
        // Comments alone (kraal's own header, say): what kraal writes goes below them, where an
        // edited document would put them last.
        let gap = match text {
            _ if text.is_empty() || text.ends_with("\n\n") => "",
            _ if text.ends_with('\n') => "\n",
            _ => "\n\n",
        };
        return Ok(format!("{text}{gap}{written}"));
    }
    // A list kraal wrote empty (`server = []`) that gets entries: they come as tables, and
    // what is written above the empty list stays on top of the file.
    let filled = |(key, item): (&str, &Item)| {
        let was_empty = item.as_array().is_some_and(toml_edit::Array::is_empty);
        (was_empty && after.get(key) != before.get(key)).then(|| key.to_string())
    };
    for key in doc.iter().filter_map(filled).collect::<Vec<_>>() {
        let above = doc.key(&key).and_then(|key| key.leaf_decor().prefix()).and_then(|raw| raw.as_str());
        let on_top = format!(
            "{}{}",
            doc.decor().prefix().and_then(|raw| raw.as_str()).unwrap_or_default(),
            above.unwrap_or_default()
        );
        doc.decor_mut().set_prefix(on_top);
        doc.remove(&key);
    }
    // What kraal would write from nothing: new keys and entries come from it, as kraal lays
    // them out and in its order.
    let fresh: DocumentMut = written.parse().context("reading what kraal writes")?;
    let mut left = String::new();
    edit_table(doc.as_table_mut(), &before, &after, fresh.as_table(), &mut left);
    if !left.is_empty() {
        let end = doc.trailing().as_str().unwrap_or_default().trim_start_matches('\n').to_string();
        doc.set_trailing(format!("{left}{end}"));
    }
    // Never a file that says something else than kraal meant: a layout this does not edit
    // right is refused, and the file stays as it is.
    let edited = doc.to_string();
    let reads = toml::from_str::<T>(&edited).ok().and_then(|read| toml::Table::try_from(&read).ok());
    ensure!(reads.as_ref() == Some(&after), "kraal cannot edit this file as it is laid out");
    Ok(edited)
}

/// Edit `doc`, read as `before`, into `after`: the keys that differ, and no other. `left` takes
/// the comments that no table is left to carry.
fn edit_table(doc: &mut dyn TableLike, before: &toml::Table, after: &toml::Table, fresh: &Table, left: &mut String) {
    for (key, fresh) in fresh.iter() {
        let (old, Some(new)) = (before.get(key), after.get(key)) else { continue };
        if old == Some(new) {
            continue;
        }
        let edited = match (doc.get_mut(key), old, new, fresh) {
            (Some(item), Some(toml::Value::Table(old)), toml::Value::Table(new), Item::Table(fresh)) => {
                item.as_table_like_mut().map(|table| edit_table(table, old, new, fresh, left)).is_some()
            }
            (Some(item), Some(toml::Value::Array(old)), toml::Value::Array(new), fresh) => {
                edit_entries(item, old, new, fresh, left) || edit_values(item, old, new, fresh)
            }
            _ => false,
        };
        if !edited {
            set(doc, key, fresh);
        }
    }
    for key in before.keys().filter(|key| !after.contains_key(*key)) {
        doc.remove(key);
    }
}

/// Give `key` the value kraal writes. A value that takes the place of another keeps what is
/// written around it (a comment at the end of its line).
fn set(doc: &mut dyn TableLike, key: &str, fresh: &Item) {
    match (doc.get_mut(key), fresh) {
        (Some(Item::Value(old)), Item::Value(new)) => {
            let around = old.decor().clone();
            *old = new.clone();
            *old.decor_mut() = around;
        }
        _ => {
            doc.insert(key, unplaced(fresh.clone()));
        }
    }
}

/// Edit a list of tables (`[[server]]` entries, or an array of inline tables), read as `before`,
/// into `after`: an entry that goes on keeps its table, edited; the others go or come. False
/// when `list` is not such a list, or not the one kraal read: nothing is edited then.
fn edit_entries(
    list: &mut Item,
    before: &[toml::Value],
    after: &[toml::Value],
    fresh: &Item,
    left: &mut String,
) -> bool {
    let tables = |values: &[toml::Value]| -> Option<Vec<toml::Table>> {
        values.iter().map(|value| value.as_table().cloned()).collect()
    };
    let (Some(before), Some(after)) = (tables(before), tables(after)) else { return false };
    let mut entries: Vec<Item> = match list {
        Item::ArrayOfTables(tables) => tables.iter().cloned().map(Item::Table).collect(),
        Item::Value(Value::Array(array)) => array.iter().cloned().map(Item::Value).collect(),
        _ => return false,
    };
    let fresh: Vec<&Table> = fresh.as_array_of_tables().map(|tables| tables.iter().collect()).unwrap_or_default();
    let read_as_before = entries.len() == before.len() && entries.iter().all(Item::is_table_like);
    if !read_as_before || fresh.len() != after.len() {
        return false;
    }
    let continued = paired(&before, &after, common);

    // What stood above an entry that goes, and was not about it, stays: above the next entry
    // that goes on, or at the end when there is none.
    let mut above = String::new();
    for (i, entry) in entries.iter_mut().enumerate() {
        let Item::Table(table) = entry else { continue };
        let (general, own) = heading(table);
        if !continued.contains(&Some(i)) {
            if general.contains('#') {
                above.push_str(if above.is_empty() { &general } else { general.trim_start_matches('\n') });
            }
        } else if !above.is_empty() {
            let prefix =
                format!("{above}{}{own}", if general.contains('#') { general.trim_start_matches('\n') } else { "" });
            table.decor_mut().set_prefix(prefix);
            above.clear();
        }
    }
    left.push_str(&above);

    let mut edited: Vec<Item> = Vec::new();
    let mut new_since = None;
    for (j, from) in continued.into_iter().enumerate() {
        let Some(i) = from else {
            new_since.get_or_insert(edited.len());
            edited.push(unplaced(Item::Table(fresh[j].clone())));
            continue;
        };
        let mut entry = std::mem::take(&mut entries[i]);
        if let Some(table) = entry.as_table_like_mut() {
            edit_table(table, &before[i], &after[j], fresh[j], left);
        }
        // New entries came in above this one: they take what stood above it and was not about
        // it (a comment for the whole file), and a blank line parts them from it.
        if let (Some(first), Item::Table(table)) = (new_since.take(), &mut entry) {
            let (general, own) = heading(table);
            if general.contains('#') || general.is_empty() {
                table.decor_mut().set_prefix(format!("\n{own}"));
            }
            if let (true, Item::Table(new)) = (general.contains('#'), &mut edited[first]) {
                new.decor_mut().set_prefix(general);
            }
        }
        edited.push(entry);
    }
    match list {
        Item::ArrayOfTables(tables) => {
            tables.clear();
            tables.extend(edited.into_iter().filter_map(|entry| entry.into_table().ok()));
        }
        Item::Value(Value::Array(array)) => {
            array.clear();
            array.extend(edited.into_iter().filter_map(|entry| entry.into_value().ok()));
        }
        _ => {}
    }
    true
}

/// What is written above a table's header, in two parts: what is not about that table (down to
/// the last blank line), and the comment right above the header, which is.
fn heading(table: &Table) -> (String, String) {
    let prefix = table.decor().prefix().and_then(|raw| raw.as_str()).unwrap_or_default();
    let own = prefix.rfind("\n\n").map_or(usize::from(prefix.starts_with('\n')), |blank| blank + 2);
    (prefix[..own].to_string(), prefix[own..].to_string())
}

/// Edit an array of plain values (the accounts of a rule), read as `before`, into `after`: a
/// value that stays keeps what is written around it, and so does one that takes the place of
/// another. False when `array` is not such an array, or not the one kraal read.
fn edit_values(array: &mut Item, before: &[toml::Value], after: &[toml::Value], fresh: &Item) -> bool {
    let (Some(array), Some(fresh)) = (array.as_array_mut(), fresh.as_array()) else { return false };
    let plain = |values: &[toml::Value]| values.iter().all(|value| !value.is_table() && !value.is_array());
    if array.len() != before.len() || fresh.len() != after.len() || !plain(before) || !plain(after) {
        return false;
    }
    let old: Vec<Value> = array.iter().cloned().collect();
    let mut values: Vec<Value> = Vec::new();
    let stays = paired(before, after, |old, new| usize::from(old == new));
    for ((new, value), from) in after.iter().zip(fresh.iter()).zip(stays) {
        let mut value = value.clone();
        match from {
            Some(i) if before[i] == *new => value = old[i].clone(),
            Some(i) => *value.decor_mut() = old[i].decor().clone(),
            None => value.decor_mut().clear(),
        }
        values.push(value);
    }
    // The first value opens the array as the first one did, whichever it is now.
    if let (Some(first), Some(opening)) = (values.first_mut(), old.first().and_then(|value| value.decor().prefix())) {
        first.decor_mut().set_prefix(opening.clone());
    }
    array.clear();
    values.into_iter().for_each(|value| array.push_formatted(value));
    true
}

/// What two entries of a list have in common, against what they differ in: more than nothing
/// for an entry that changed (a server given `agents`, an account renamed), nothing for one that
/// came in next to it.
fn common(old: &toml::Table, new: &toml::Table) -> usize {
    let both = old.keys().filter(|key| new.contains_key(*key)).count();
    let same = old.iter().filter(|(key, value)| new.get(*key) == Some(*value)).count();
    (2 * same).saturating_sub(old.len() + new.len() - both - same)
}

/// For each of `after`, the one of `before` it continues, if any. They pair in order, so that
/// the pairs have as much in `common` as can be. Between two such pairs, what is left pairs in
/// order too, from the end: an entry that changed beyond recognition is still where it was,
/// below what came in above it.
fn paired<T: PartialEq>(before: &[T], after: &[T], common: impl Fn(&T, &T) -> usize) -> Vec<Option<usize>> {
    // What did not change at either end continues itself: what is left to pair is small.
    let same = |(old, new): &(&T, &T)| old == new;
    let head = before.iter().zip(after).take_while(same).count();
    let tail = before[head..].iter().rev().zip(after[head..].iter().rev()).take_while(same).count();
    let (old, new) = (&before[head..before.len() - tail], &after[head..after.len() - tail]);
    // best[i][j]: the most that old[i..] and new[j..] can keep in common.
    let mut best = vec![vec![0; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            let together = match common(&old[i], &new[j]) {
                0 => 0,
                kept => kept + best[i + 1][j + 1],
            };
            best[i][j] = together.max(best[i + 1][j]).max(best[i][j + 1]);
        }
    }
    let mut pairs: Vec<Option<usize>> = (0..head).map(Some).collect();
    pairs.resize(head + new.len(), None);
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        let kept = common(&old[i], &new[j]);
        if kept > 0 && best[i][j] == kept + best[i + 1][j + 1] {
            pairs[head + j] = Some(head + i);
            (i, j) = (i + 1, j + 1);
        } else if best[i][j] == best[i + 1][j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let mut unpaired = Vec::new();
    let mut passed = head;
    for j in head..=head + new.len() {
        let until = match pairs.get(j) {
            Some(Some(i)) => *i,
            Some(None) => {
                unpaired.push(j);
                continue;
            }
            None => before.len() - tail,
        };
        for (i, j) in (passed..until).rev().zip(unpaired.drain(..).rev()) {
            pairs[j] = Some(i);
        }
        passed = until + 1;
    }
    pairs.extend((before.len() - tail..before.len()).map(Some));
    pairs
}

/// `item`, read in what kraal writes, as something new in the user's file: its tables have no
/// place of their own there (each comes after what precedes it) and no spacing but the usual.
fn unplaced(mut item: Item) -> Item {
    struct Unplaced;
    impl VisitMut for Unplaced {
        fn visit_table_mut(&mut self, table: &mut Table) {
            table.set_position(None);
            table.decor_mut().clear();
            visit_mut::visit_table_mut(self, table);
        }
    }
    Unplaced.visit_item_mut(&mut item);
    item
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    struct File {
        #[serde(default)]
        limits: Limits,
        #[serde(default, rename = "server")]
        servers: Vec<Server>,
        #[serde(default, rename = "rule")]
        rules: Vec<Rule>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Limits {
        threshold: u8,
    }

    impl Default for Limits {
        fn default() -> Self {
            Limits { threshold: 90 }
        }
    }

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    struct Server {
        name: String,
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agents: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        groups: Vec<String>,
    }

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    struct Rule {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        accounts: Vec<String>,
    }

    fn server(name: &str) -> Server {
        Server { name: name.into(), host: format!("{name}.example.com"), ..Server::default() }
    }

    /// `text` after `change` to what kraal read in it; what kraal reads in the result is checked
    /// to be what it wrote.
    fn rewritten(text: &str, change: impl FnOnce(&mut File)) -> String {
        let mut file: File = toml::from_str(text).unwrap();
        change(&mut file);
        let out = super::rewrite(text, &file).unwrap();
        assert_eq!(toml::from_str::<File>(&out).unwrap(), file, "{out}");
        out
    }

    const INVENTORY: &str = "\
# My servers, by hand.

[[server]]
name = \"bastion\"            # the way in
host = \"bastion.example.com\"
color = \"red\"               # a key of another kraal

# The web servers.
[[server]]
host = \"web.example.com\"
name = \"web\"
groups = [
    \"web\",   # behind the balancer
]

# Kept for later:
# [[server]]
# name = \"old\"
";

    #[test]
    fn nothing_changed_is_the_text_as_it_is() {
        assert_eq!(rewritten(INVENTORY, |_| {}), INVENTORY);
        assert_eq!(rewritten("# only a comment", |_| {}), "# only a comment");
    }

    #[test]
    fn a_key_set_in_an_entry_leaves_the_rest_of_the_file() {
        let out = rewritten(INVENTORY, |file| file.servers[0].agents = Some("main".into()));
        assert_eq!(
            out,
            INVENTORY.replace("# a key of another kraal\n", "# a key of another kraal\nagents = \"main\"\n")
        );
        // A value that changes keeps its place and the comment on its line; one that goes, goes.
        let out = rewritten(INVENTORY, |file| {
            file.servers[0].host = "gate.example.com".into();
            file.servers[1].groups.clear();
        });
        let expected = INVENTORY
            .replace("host = \"bastion.example.com\"", "host = \"gate.example.com\"")
            .replace("groups = [\n    \"web\",   # behind the balancer\n]\n", "");
        assert_eq!(out, expected);
    }

    #[test]
    fn entries_come_and_go_among_the_others() {
        // A new entry comes last among its kind, laid out as kraal writes one.
        let out = rewritten(INVENTORY, |file| file.servers.push(Server { groups: vec!["db".into()], ..server("db") }));
        let added = "\n[[server]]\nname = \"db\"\nhost = \"db.example.com\"\ngroups = [\"db\"]\n";
        assert_eq!(out, INVENTORY.replace("\n# Kept for later:", &format!("{added}\n# Kept for later:")));
        // An entry that goes takes the comment right above it along; what stood higher, for the
        // whole file, stays above the next entry.
        let web = INVENTORY.split_once("# The web servers.\n").unwrap().1;
        let out = rewritten(INVENTORY, |file| {
            file.servers.remove(0);
        });
        assert_eq!(out, format!("# My servers, by hand.\n\n# The web servers.\n{web}"));
        // The last one gone, the comments that were about none stay.
        let out = rewritten(INVENTORY, |file| file.servers.clear());
        assert_eq!(out, "# My servers, by hand.\n\n# Kept for later:\n# [[server]]\n# name = \"old\"\n");
    }

    #[test]
    fn a_new_table_and_a_first_entry_go_below_what_is_there() {
        let out = rewritten(INVENTORY, |file| file.limits.threshold = 80);
        assert_eq!(out, INVENTORY.replace("\n# Kept for later:", "\n[limits]\nthreshold = 80\n\n# Kept for later:"));
        let out = rewritten("# Nothing yet.\n", |file| file.servers.push(server("web")));
        let whole = "rule = []\n\n[limits]\nthreshold = 90\n\n[[server]]\nname = \"web\"\nhost = \"web.example.com\"\n";
        assert_eq!(out, format!("# Nothing yet.\n\n{whole}"));
        // A list kraal wrote empty takes entries as tables.
        let out =
            rewritten("server = []\n\n[limits]\nthreshold = 70 # mine\n", |file| file.servers.push(server("web")));
        assert_eq!(
            out,
            "\n[limits]\nthreshold = 70 # mine\n\n[[server]]\nname = \"web\"\nhost = \"web.example.com\"\n"
        );
        let out = rewritten("# Mine.\n\nserver = []\n", |file| file.servers.push(server("web")));
        assert_eq!(out, "# Mine.\n\n[[server]]\nname = \"web\"\nhost = \"web.example.com\"\n");
    }

    #[test]
    fn a_changed_entry_keeps_its_table_whatever_changed() {
        let rules = "\
[[rule]]
workspace = \"shop\"
accounts = [\"pro\", \"default\"]  # work first
priority = 1

[[rule]]
workspace = \"blog\"
accounts = [\"default\"]
";
        // The rule goes on without an account, and one for an agent alone comes before it.
        let out = rewritten(rules, |file| {
            file.rules[0].accounts.remove(0);
            let own =
                Rule { workspace: Some("shop".into()), agent: Some("claude".into()), accounts: vec!["work".into()] };
            file.rules.insert(0, own);
        });
        let expected = "\
[[rule]]
workspace = \"shop\"
agent = \"claude\"
accounts = [\"work\"]

[[rule]]
workspace = \"shop\"
accounts = [\"default\"]  # work first
priority = 1

[[rule]]
workspace = \"blog\"
accounts = [\"default\"]
";
        assert_eq!(out, expected);
        // An entry changed beyond recognition is still where it was, with what kraal does not
        // read in it.
        let out = rewritten(INVENTORY, |file| file.servers[0] = server("db"));
        let expected = INVENTORY
            .replace("name = \"bastion\"", "name = \"db\"")
            .replace("host = \"bastion.example.com\"", "host = \"db.example.com\"");
        assert_eq!(out, expected);
    }

    #[test]
    fn a_list_of_values_is_edited_value_by_value() {
        let rule = "\
[[rule]]
workspace = \"shop\"
accounts = [
    \"pro\",      # the team's
    \"default\",  # mine
]
";
        // A value that changes keeps its place and what is written after it.
        let out = rewritten(rule, |file| file.rules[0].accounts[0] = "work".into());
        assert_eq!(out, rule.replace("\"pro\"", "\"work\""));
        // One that goes takes its comment along; the other stays as it is.
        let out = rewritten(rule, |file| {
            file.rules[0].accounts.remove(0);
        });
        assert_eq!(out, rule.replace("    \"pro\",      # the team's\n", ""));
        let one_line = "[[rule]]\naccounts = [\"pro\", \"default\"] # in this order\n";
        let out = rewritten(one_line, |file| {
            file.rules[0].accounts.remove(0);
        });
        assert_eq!(out, "[[rule]]\naccounts = [\"default\"] # in this order\n");
        let out = rewritten(one_line, |file| file.rules[0].accounts.push("spare".into()));
        assert_eq!(out, "[[rule]]\naccounts = [\"pro\", \"default\", \"spare\"] # in this order\n");
    }

    #[test]
    fn an_array_of_inline_tables_keeps_its_entries_too() {
        let inline = "server = [\n    { name = \"a\", host = \"a.example.com\", color = \"red\" }, # first\n    { name = \"b\", host = \"b.example.com\" },\n]\n";
        let out = rewritten(inline, |file| {
            file.servers[0].agents = Some("main".into());
            file.servers.remove(1);
        });
        assert!(
            out.contains("color = \"red\"") && out.contains("agents = \"main\"") && !out.contains("b.example"),
            "{out}"
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_error() {
        assert!(super::rewrite::<File>("[[server]\nname = 1", &File::default()).is_err());
    }
}
