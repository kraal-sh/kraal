//! The pages against the program: what README.md and docs/ quote (command lines, keys, settings,
//! event types, examples) is read from the pages and given to the program's own parsers, so that
//! a page which no longer matches the program fails here, and so does a command, an option, a
//! setting or an event type that no page names.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser as _};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use super::args::Cli;
use crate::config::{Config, DEFAULT_CONFIG};
use crate::input::{Key, KeyCode, KeySpec};
use crate::keymap::{Command, Keymap, Lookup};

/// Code a page quotes.
struct Quote {
    /// Where: `docs/keys.md:42`.
    at: String,
    /// The language of a fenced block; `None` for a span of a line.
    block: Option<String>,
    text: String,
}

#[derive(Default)]
struct Page {
    /// `README.md`, `docs/keys.md`.
    name: String,
    quotes: Vec<Quote>,
    /// Its tables: rows (the head first) of cells, each cell the spans it quotes.
    tables: Vec<Vec<Vec<Vec<String>>>>,
    /// What a link can point at in it: its headings, as GitHub names them.
    anchors: BTreeSet<String>,
    /// Its links: where each is written, and where it leads.
    links: Vec<(String, String)>,
}

fn pages() -> Vec<Page> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut docs: Vec<PathBuf> = std::fs::read_dir(root.join("docs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "md"))
        .collect();
    docs.sort();
    std::iter::once(root.join("README.md")).chain(docs).map(|path| page(root, &path)).collect()
}

fn page(root: &Path, path: &Path) -> Page {
    let text = std::fs::read_to_string(path).unwrap();
    let name = path.strip_prefix(root).unwrap().display().to_string();
    let at = |offset: usize| format!("{name}:{}", text[..offset].matches('\n').count() + 1);
    let mut page = Page { name: name.clone(), ..Page::default() };
    let mut block: Option<Quote> = None;
    let mut heading: Option<String> = None;
    let mut in_cell = false;
    for (event, range) in Parser::new_ext(&text, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Fenced(language) => language.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                block = Some(Quote { at: at(range.start), block: Some(language), text: String::new() });
            }
            Event::Text(part) => {
                if let Some(written) = block.as_mut().map(|quote| &mut quote.text).or(heading.as_mut()) {
                    written.push_str(&part);
                }
            }
            Event::End(TagEnd::CodeBlock) => page.quotes.extend(block.take()),
            Event::Code(span) => {
                // A span broken over two lines reads as one.
                let span = span.split_whitespace().collect::<Vec<_>>().join(" ");
                let cell = page.tables.last_mut().and_then(|t| t.last_mut()).and_then(|row| row.last_mut());
                if let Some(cell) = cell.filter(|_| in_cell) {
                    cell.push(span.clone());
                }
                if let Some(heading) = &mut heading {
                    heading.push_str(&span);
                }
                page.quotes.push(Quote { at: at(range.start), block: None, text: span });
            }
            Event::Start(Tag::Table(_)) => page.tables.push(Vec::new()),
            Event::Start(Tag::TableHead | Tag::TableRow) => page.tables.last_mut().unwrap().push(Vec::new()),
            Event::Start(Tag::TableCell) => {
                page.tables.last_mut().unwrap().last_mut().unwrap().push(Vec::new());
                in_cell = true;
            }
            Event::End(TagEnd::TableCell) => in_cell = false,
            Event::Start(Tag::Heading { .. }) => heading = Some(String::new()),
            Event::End(TagEnd::Heading(_)) => page.anchors.extend(heading.take().map(|title| anchor(&title))),
            Event::Start(Tag::Link { dest_url, .. }) => page.links.push((at(range.start), dest_url.to_string())),
            // The README's header is HTML.
            Event::Html(html) | Event::InlineHtml(html) => {
                let links = html.split("href=\"").skip(1).filter_map(|rest| rest.split('"').next());
                page.links.extend(links.map(|link| (at(range.start), link.to_string())));
            }
            _ => {}
        }
    }
    page
}

/// The anchor GitHub gives a heading: lower case, spaces as hyphens, other punctuation left out.
fn anchor(title: &str) -> String {
    let kept = title.to_lowercase().replace(' ', "-");
    kept.chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_')).collect()
}

fn quotes(pages: &[Page]) -> impl Iterator<Item = &Quote> {
    pages.iter().flat_map(|page| &page.quotes)
}

/// The fenced blocks of a language, each with what its opening line says after the language.
fn blocks<'a>(pages: &'a [Page], language: &'a str) -> impl Iterator<Item = (&'a Quote, &'a str)> {
    quotes(pages).filter_map(move |quote| {
        let opening = quote.block.as_deref()?;
        let (first, rest) = opening.split_once(' ').unwrap_or((opening, ""));
        (first == language).then_some((quote, rest.trim()))
    })
}

// ---- command lines -----------------------------------------------------------------------

/// The commands of a shell line, each as its words: quotes are read, `#` starts a comment, a
/// pipe or `;` starts the next command, and what a redirection names is left out.
fn commands(line: &str) -> Vec<Vec<String>> {
    let line = line.trim_start();
    let mut out = vec![Vec::new()];
    let mut word: Option<String> = None;
    let mut redirected = false;
    let mut chars = line.strip_prefix("$ ").unwrap_or(line).chars();
    loop {
        let c = chars.next();
        match c {
            Some('\'') => word.get_or_insert_default().extend(chars.by_ref().take_while(|c| *c != '\'')),
            Some('"') => {
                let word = word.get_or_insert_default();
                while let Some(c) = chars.next().filter(|c| *c != '"') {
                    word.extend(if c == '\\' { chars.next() } else { Some(c) });
                }
            }
            Some('\\') => word.get_or_insert_default().extend(chars.next()),
            Some('#') if word.is_none() => break,
            Some(c) if !c.is_whitespace() && !"|;<>".contains(c) => word.get_or_insert_default().push(c),
            _ => {
                if let Some(word) = word.take().filter(|_| !std::mem::take(&mut redirected)) {
                    out.last_mut().unwrap().push(word);
                }
                match c {
                    Some('|' | ';') => out.push(Vec::new()),
                    Some('<' | '>') => redirected = true,
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }
    out
}

/// The `kraal` command lines of a quote: a span that starts with `kraal`, and each line of a
/// shell block or a transcript that runs it.
fn command_lines(quote: &Quote) -> Vec<Vec<String>> {
    let runs: Vec<Vec<String>> = match quote.block.as_deref() {
        None => commands(&quote.text).into_iter().take(1).collect(),
        Some("sh" | "console" | "") => quote.text.lines().flat_map(commands).collect(),
        Some(_) => Vec::new(),
    };
    runs.into_iter().filter(|words| words.first().is_some_and(|w| w == "kraal")).collect()
}

/// The values a command's first argument chooses from (`kraal config` takes `check`, `keys`…).
fn choices(cmd: &clap::Command) -> Vec<String> {
    let first = cmd.get_positionals().next();
    first.map(|arg| arg.get_possible_values().iter().map(|v| v.get_name().to_string()).collect()).unwrap_or_default()
}

/// What a command line names of the program: its command (`kraal buffer list`, `kraal config
/// check`) and each option it gives, by its long name (`kraal buffer list --json`).
fn named(words: &[String]) -> Vec<String> {
    let root = Cli::command();
    let (mut cmd, mut path) = (&root, String::from("kraal"));
    let mut out = Vec::new();
    let mut words = words[1..].iter();
    while let Some(word) = words.next().filter(|word| *word != "--") {
        if let Some(sub) = cmd.find_subcommand(word) {
            (cmd, path) = (sub, format!("{path} {}", sub.get_name()));
            out.push(path.clone());
            continue;
        }
        if choices(cmd).contains(word) {
            out.push(format!("{path} {word}"));
            continue;
        }
        let flag = word.split('=').next().unwrap_or(word);
        let (long, short) = (flag.strip_prefix("--"), flag.strip_prefix('-').filter(|f| f.chars().count() == 1));
        // An option of the command, else one every command takes (`-s`).
        let option = [(cmd, path.as_str()), (&root, "kraal")].into_iter().find_map(|(cmd, path)| {
            let is = |arg: &&clap::Arg| {
                long.is_some_and(|l| arg.get_long() == Some(l))
                    || short.is_some_and(|s| arg.get_short().is_some_and(|c| s.starts_with(c)))
            };
            cmd.get_arguments().find(is).map(|arg| (arg, path))
        });
        if let Some((arg, path)) = option {
            out.push(format!("{path} --{}", arg.get_long().unwrap_or_default()));
            if arg.get_action().takes_values() && !word.contains('=') {
                words.next();
            }
        }
    }
    out
}

/// Every command and option `--help` shows from `cmd` down, written as [`named`] writes them.
fn listed(cmd: &clap::Command, path: &str, out: &mut Vec<String>) {
    out.extend(
        cmd.get_arguments().filter(|a| !a.is_hide_set()).filter_map(|a| Some(format!("{path} --{}", a.get_long()?))),
    );
    out.extend(choices(cmd).into_iter().map(|choice| format!("{path} {choice}")));
    for sub in cmd.get_subcommands().filter(|sub| !sub.is_hide_set()) {
        let path = format!("{path} {}", sub.get_name());
        out.push(path.clone());
        listed(sub, &path, out);
    }
}

#[test]
fn shell_lines_split_into_commands() {
    let words = |line: &str| commands(line).into_iter().map(|c| c.join(",")).collect::<Vec<_>>();
    assert_eq!(
        words("kraal exec web01 db -- 'df -h | grep /dev'   # on named servers"),
        ["kraal,exec,web01,db,--,df -h | grep /dev"]
    );
    assert_eq!(words("git diff | kraal buffer set review"), ["git,diff", "kraal,buffer,set,review"]);
    assert_eq!(words("$ kraal buffer set < out.txt"), ["kraal,buffer,set"]);
    assert_eq!(
        words("kraal send -p tests --enter \"cargo \\\"test\\\"\""),
        ["kraal,send,-p,tests,--enter,cargo \"test\""]
    );
    assert_eq!(words("kraal tunnel rm #7"), ["kraal,tunnel,rm"]);
    let named = |line: &str| named(&commands(line).remove(0)).join(", ");
    assert_eq!(named("kraal -s main read -p 3"), "kraal --session, kraal read, kraal read --pane");
    assert_eq!(
        named("kraal run -x --cwd=/tmp -- cargo test --release"),
        "kraal run, kraal run --close-on-exit, kraal run --cwd"
    );
    assert_eq!(named("kraal config keys --markdown"), "kraal config, kraal config keys, kraal config --markdown");
}

/// The spans the default configuration quotes in its comments: `kraal config default` is a page
/// too.
fn default_config_quotes() -> Vec<Quote> {
    let comments = DEFAULT_CONFIG.lines().enumerate().filter(|(_, line)| line.starts_with('#'));
    let spans = comments.flat_map(|(i, line)| line.split('`').skip(1).step_by(2).map(move |span| (i + 1, span)));
    spans
        .map(|(line, span)| Quote { at: format!("kraal config default:{line}"), block: None, text: span.into() })
        .collect()
}

#[test]
fn the_command_lines_quoted_parse() {
    let mut wrong = Vec::new();
    for quote in quotes(&pages()).chain(&default_config_quotes()) {
        for words in command_lines(quote) {
            let Err(e) = Cli::try_parse_from(&words) else { continue };
            // A span may name a command without its arguments, and `…` stands for some.
            let partial = quote.block.is_none() || words.iter().any(|w| w == "…");
            let incomplete = matches!(
                e.kind(),
                ErrorKind::MissingRequiredArgument
                    | ErrorKind::MissingSubcommand
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    | ErrorKind::TooFewValues
                    | ErrorKind::WrongNumberOfValues
            );
            if !(partial && incomplete) {
                let said = e.to_string();
                wrong.push(format!("{}: `{}`: {}", quote.at, words.join(" "), said.lines().next().unwrap_or_default()));
            }
        }
    }
    assert!(wrong.is_empty(), "command lines the program does not take:\n{}", wrong.join("\n"));
}

/// Left out of the pages on purpose: they name no other program of kraal's kind.
const UNQUOTED: &str = "kraal import-zellij";

/// What a page quotes of the program: what its command lines name, and the options it quotes
/// alone (`--check`, `--to v0.2.0`).
fn quoted(page: &Page) -> (BTreeSet<String>, BTreeSet<&str>) {
    let lines = page.quotes.iter().flat_map(command_lines);
    let spans = page.quotes.iter().filter(|quote| quote.block.is_none());
    let options = spans.filter_map(|quote| quote.text.split([' ', '=']).next().filter(|word| word.starts_with("--")));
    (lines.flat_map(|words| named(&words)).collect(), options.collect())
}

/// Every command `--help` lists is quoted by a page, and every option too: in a command line, or
/// alone on a page that quotes its command.
#[test]
fn every_command_and_option_is_quoted() {
    let pages = pages();
    let quoted: Vec<_> = pages.iter().map(quoted).collect();
    let mut all = Vec::new();
    listed(&Cli::command(), "kraal", &mut all);
    let found = |name: &String| {
        let alone = name.split_once(" --").map(|(command, _)| (command, &name[command.len() + 1..]));
        quoted.iter().any(|(named, options)| {
            named.contains(name)
                || alone.is_some_and(|(command, option)| named.contains(command) && options.contains(option))
        })
    };
    let missing: Vec<String> = all.into_iter().filter(|name| !name.starts_with(UNQUOTED) && !found(name)).collect();
    assert!(missing.is_empty(), "no page quotes:\n{}", missing.join("\n"));
}

// ---- keys --------------------------------------------------------------------------------

fn keymap() -> Keymap {
    let (keymap, warnings) = Keymap::build(&Config::default().keys);
    assert!(warnings.is_empty(), "{warnings:?}");
    keymap
}

/// A key as a page writes it (`S`, `alt+↑`, `→`), if it is one.
fn key(written: &str) -> Option<KeySpec> {
    let named = [('←', "left"), ('→', "right"), ('↑', "up"), ('↓', "down")]
        .iter()
        .fold(written.to_string(), |key, (arrow, name)| key.replace(*arrow, name));
    KeySpec::parse(&named).ok()
}

/// The keys of a sequence as a page writes it (`P s →`), if every word of it is a key.
fn sequence(written: &str) -> Option<Vec<Key>> {
    let pressed = |spec: KeySpec| Key { code: spec.code, mods: spec.mods, raw: Vec::new() };
    written.split_whitespace().map(|word| key(word).map(pressed)).collect()
}

#[test]
fn the_key_sequences_quoted_are_bound() {
    let keymap = keymap();
    let prefix = keymap.prefix_label();
    let mut unbound = Vec::new();
    for quote in quotes(&pages()).filter(|quote| quote.block.is_none()) {
        let after = quote.text.strip_prefix(&prefix).filter(|rest| rest.is_empty() || rest.starts_with(' '));
        // A span that is not all keys (`alt+arrows`) is not a sequence to look up.
        let bound = match after {
            Some(after) => sequence(after).is_none_or(|keys| keymap.lookup(&keys) != Lookup::Nothing),
            None if quote.text.starts_with("alt+") => {
                sequence(&quote.text).is_none_or(|keys| keys.len() != 1 || keymap.direct(&keys[0]).is_some())
            }
            None => true,
        };
        if !bound {
            unbound.push(format!("{}: `{}`", quote.at, quote.text));
        }
    }
    assert!(unbound.is_empty(), "keys the default keymap does not bind:\n{}", unbound.join("\n"));
}

/// The tables of docs/keys.md that list the keys after the prefix: one row per theme (its
/// letter, then its menu's key and what the menu shows), and the other keys.
#[test]
fn the_tables_of_keys_are_the_menu() {
    let keymap = keymap();
    let pages = pages();
    let page = pages.iter().find(|page| page.name.ends_with("keys.md")).unwrap();
    let name = |key: &KeySpec| key.to_string();
    let keys = |cell: &[String]| -> Vec<String> {
        cell.iter().map(|k| name(&key(k).unwrap_or_else(|| panic!("{k:?} is not a key")))).collect()
    };
    let shows = |typed: &[Key]| -> Vec<crate::keymap::MenuEntry> { keymap.menu(typed) };

    let themes = page.tables.iter().find(|table| table[0].len() == 3).expect("the table of themes");
    assert_eq!(themes.len() - 1, keymap.menus.len(), "one row per menu");
    for row in &themes[1..] {
        let (letter, menu_keys) = (keys(&row[1][..1]), keys(&row[2]));
        let menu = keymap.menus.iter().find(|m| m.keys.first().map(name).as_ref() == menu_keys.first());
        let menu = menu.unwrap_or_else(|| panic!("no menu opens with {:?}", menu_keys.first()));
        let main = sequence(&letter[0]).unwrap();
        assert_eq!(keymap.lookup(&main), Lookup::Run(Command::Action(menu.main)), "{}", menu.title);
        let opened = sequence(&menu_keys[0]).unwrap();
        let shown: BTreeSet<String> = shows(&opened).iter().map(|entry| name(&entry.key)).collect();
        let listed: BTreeSet<String> = menu_keys[1..].iter().cloned().collect();
        assert_eq!(listed, shown, "the {} menu", menu.title);
    }

    let others = page.tables.iter().find(|table| table[0].len() == 2 && table[0][0] == [keymap.prefix_label()]);
    let listed: BTreeSet<String> =
        others.expect("the table of the other keys")[1..].iter().flat_map(|row| keys(&row[0])).collect();
    for key in &listed {
        assert_ne!(keymap.lookup(&sequence(key).unwrap()), Lookup::Nothing, "{key} after the prefix");
    }
    // Arrows and digits are written as a range there.
    let ranged = |key: &KeySpec| {
        matches!(key.code, KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down)
            || matches!(key.code, KeyCode::Char(c) if c.is_ascii_digit())
    };
    let missing: Vec<String> = shows(&[])
        .iter()
        .filter(|entry| !entry.main && !entry.group && !ranged(&entry.key))
        .map(|entry| name(&entry.key))
        .filter(|key| !listed.contains(key))
        .collect();
    assert!(missing.is_empty(), "keys after the prefix that the table leaves out: {missing:?}");
}

// ---- settings and examples ---------------------------------------------------------------

/// Settings the default configuration shows commented out: they have no default.
const UNSET: [&str; 8] = [
    "shell",
    "theme.accent",
    "theme.dim",
    "theme.sync",
    "theme.production",
    "theme.frames",
    "worktrees.port_base",
    "hints.patterns",
];

/// Tables whose keys are names the user gives or chooses among (a tool, an event, a command):
/// reading the configuration checks those.
const NAMED: [&str; 5] = ["tools", "hints.patterns", "hooks", "keys.leader", "keys.direct"];

/// Every setting, as a configuration that sets them all.
fn settings() -> toml::Table {
    let mut all: toml::Table = DEFAULT_CONFIG.parse().unwrap();
    for name in UNSET {
        let (table, key) = name.rsplit_once('.').map_or((None, name), |(table, key)| (Some(table), key));
        let table = match table {
            Some(table) => all[table].as_table_mut().unwrap(),
            None => &mut all,
        };
        table.insert(key.into(), toml::Value::Boolean(true));
    }
    all
}

/// The settings of `given` that `known` does not have, by their path (`sidebar.width`).
fn unknown(given: &toml::Table, known: &toml::Table, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (key, value) in given {
        let path = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
        match (value, known.get(key)) {
            (_, None) => out.push(path),
            (toml::Value::Table(given), Some(toml::Value::Table(known))) if !NAMED.contains(&path.as_str()) => {
                out.extend(unknown(given, known, &path));
            }
            _ => {}
        }
    }
    out
}

/// The keys of `given` (a file as a page shows it) that `kept` (the file as kraal writes it back)
/// lost: kraal did not read them. `path` is the keys down to `given`, a space before each.
fn dropped(given: &toml::Value, kept: Option<&toml::Value>, path: &str) -> Vec<String> {
    match (given, kept) {
        (_, None) => vec![format!("kraal does not read{path}")],
        (toml::Value::Table(given), Some(kept)) => {
            given.iter().flat_map(|(key, value)| dropped(value, kept.get(key), &format!("{path} {key}"))).collect()
        }
        (toml::Value::Array(given), Some(kept)) => {
            given.iter().enumerate().flat_map(|(i, value)| dropped(value, kept.get(i), path)).collect()
        }
        _ => Vec::new(),
    }
}

/// What is wrong with a `config.toml` a page shows: what `kraal config check` would say, and the
/// settings kraal does not have (it reads past them).
fn config_problems(text: &str, table: &toml::Table) -> Vec<String> {
    let config = match Config::from_user_toml(text) {
        Ok(config) => config,
        Err(e) => return vec![format!("{e:#}")],
    };
    let unknown = unknown(table, &settings(), "").into_iter().map(|name| format!("no setting {name}"));
    super::config_warnings(&config).into_iter().chain(unknown).collect()
}

/// What is wrong with a file a page shows, once written at `path`: kraal reads it with `load`
/// and writes it back with `save`, as it does when it changes the file.
fn file_problems<T>(
    table: &toml::Table,
    path: &Path,
    load: impl Fn(&Path) -> anyhow::Result<T>,
    save: impl Fn(&T) -> anyhow::Result<()>,
) -> Vec<String> {
    match load(path).and_then(|file| save(&file)) {
        Ok(()) => {
            let kept: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
            dropped(&toml::Value::Table(table.clone()), Some(&toml::Value::Table(kept)), "")
        }
        Err(e) => vec![format!("{e:#}")],
    }
}

/// Each TOML example says after `toml` which file it shows (GitHub colours it all the same), and
/// goes to that file's parser.
#[test]
fn the_toml_examples_are_read_whole() {
    let dir = std::env::temp_dir().join(format!("kraal-docs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("example.toml");
    let failed = |read: anyhow::Result<()>| read.err().map(|e| format!("{e:#}")).into_iter().collect();
    let mut wrong = Vec::new();
    for (quote, file) in blocks(&pages(), "toml") {
        let table: toml::Table = quote.text.parse().unwrap_or_else(|e| panic!("{}: {e}", quote.at));
        std::fs::write(&path, &quote.text).unwrap();
        let problems: Vec<String> = match file {
            "config" => config_problems(&quote.text, &table),
            "servers" => {
                file_problems(&table, &path, crate::servers::Inventory::load_from, crate::servers::Inventory::save)
            }
            "accounts" => {
                file_problems(&table, &path, crate::accounts::Accounts::load_from, crate::accounts::Accounts::save)
            }
            "runbook" => failed(crate::runbooks::load(&path, crate::runbooks::Source::User).map(drop)),
            "worktree" => failed(crate::worktree_setup::Setup::parse(&quote.text).map(drop)),
            other => vec![format!("`toml {other}`: say config, servers, accounts, runbook or worktree")],
        };
        wrong.extend(problems.into_iter().map(|problem| format!("{}: {problem}", quote.at)));
    }
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(wrong.is_empty(), "examples kraal would not read as written:\n{}", wrong.join("\n"));
}

/// docs/configuration.md is the reference: its examples together set every setting.
#[test]
fn the_reference_names_every_setting() {
    let pages = pages();
    let reference: Vec<Page> = pages.into_iter().filter(|page| page.name.ends_with("configuration.md")).collect();
    let mut named = toml::Table::new();
    for (quote, _) in blocks(&reference, "toml").filter(|(_, file)| *file == "config") {
        merge(&mut named, quote.text.parse().unwrap());
    }
    let missing = unknown(&settings(), &named, "");
    assert!(missing.is_empty(), "settings docs/configuration.md does not show: {missing:?}");
}

/// `from` into `into`, table by table.
fn merge(into: &mut toml::Table, from: toml::Table) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(toml::Value::Table(into)), toml::Value::Table(from)) => merge(into, from),
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

#[test]
fn the_workspace_examples_have_no_warning() {
    for (quote, _) in blocks(&pages(), "kdl") {
        let workspace = crate::workspace::parse_workspace("example", &quote.text);
        let workspace = workspace.unwrap_or_else(|e| panic!("{}: {e:#}", quote.at));
        assert!(workspace.warnings.is_empty(), "{}: {:?}", quote.at, workspace.warnings);
    }
}

// ---- links and versions ------------------------------------------------------------------

/// `path` without its `..` (`docs/../README.md` is `README.md`).
fn plain(path: &Path) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.iter().filter_map(|part| part.to_str()) {
        if part == ".." {
            parts.pop();
        } else {
            parts.push(part);
        }
    }
    parts.join("/")
}

#[test]
fn the_links_between_pages_lead_somewhere() {
    let pages = pages();
    let mut broken = Vec::new();
    for page in &pages {
        let links = page.links.iter().filter(|(_, link)| !link.contains(':'));
        for (at, link) in links {
            let (file, heading) = link.split_once('#').map_or((link.as_str(), None), |(f, h)| (f, Some(h)));
            let name = match file {
                "" => page.name.clone(),
                file => plain(&Path::new(&page.name).with_file_name(file)),
            };
            // Other files (the licence, images) are not pages.
            if !name.ends_with(".md") {
                continue;
            }
            let target = pages.iter().find(|page| page.name == name);
            if !target.is_some_and(|page| heading.is_none_or(|h| page.anchors.contains(h))) {
                broken.push(format!("{at}: {link}"));
            }
        }
    }
    assert!(broken.is_empty(), "links to a page or a heading that is not there:\n{}", broken.join("\n"));
}

/// The oldest release kraal works with is written twice: tests/compat.sh tests it, and
/// docs/compatibility.md promises it.
#[test]
fn the_oldest_release_promised_is_the_one_tested() {
    let read = |name: &str| std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(name)).unwrap();
    let script = read("tests/compat.sh");
    let oldest = script.lines().find_map(|line| line.strip_prefix("OLDEST=v")).expect("OLDEST in tests/compat.sh");
    let promised = format!("from **{oldest}** on");
    assert!(read("docs/compatibility.md").contains(&promised), "docs/compatibility.md does not say {promised:?}");
}

// ---- events ------------------------------------------------------------------------------

/// The event types a span names, when it starts like one: `pane.opened/exited` names two, and
/// `agent.*` those of agents.
fn event_types(span: &str) -> Vec<String> {
    let Some((of, names)) = span.split_once('.') else { return Vec::new() };
    let types = crate::server::events::TYPES;
    if !types.iter().any(|ty| ty.split_once('.').is_some_and(|(kind, _)| kind == of)) {
        return Vec::new();
    }
    names.split('/').map(|name| format!("{of}.{}", name.trim())).collect()
}

#[test]
fn the_event_types_quoted_are_the_programs() {
    let pages = pages();
    let types = crate::server::events::TYPES;
    let spans = |pages: &[&Page]| -> Vec<(String, String)> {
        let spans = pages.iter().flat_map(|page| &page.quotes).filter(|quote| quote.block.is_none());
        spans.flat_map(|quote| event_types(&quote.text).into_iter().map(|ty| (quote.at.clone(), ty))).collect()
    };
    let wrong: Vec<String> = spans(&pages.iter().collect::<Vec<_>>())
        .into_iter()
        .filter(|(_, ty)| !ty.ends_with(".*") && !types.contains(&ty.as_str()))
        .map(|(at, ty)| format!("{at}: {ty}"))
        .collect();
    assert!(wrong.is_empty(), "event types kraal does not send:\n{}", wrong.join("\n"));

    let scripting: Vec<&Page> = pages.iter().filter(|page| page.name.ends_with("scripting.md")).collect();
    let listed: BTreeSet<String> = scripting
        .iter()
        .flat_map(|page| &page.quotes)
        .filter(|quote| quote.block.is_none())
        .map(|quote| quote.text.clone())
        .chain(spans(&scripting).into_iter().map(|(_, ty)| ty))
        .collect();
    let missing: Vec<&&str> = types.iter().filter(|ty| !listed.contains(**ty)).collect();
    assert!(missing.is_empty(), "event types docs/scripting.md does not list: {missing:?}");
}
