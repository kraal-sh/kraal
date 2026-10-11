//! Tests of the file manager: listings, keys, rendering and remote parsing.

use super::listing::*;
use super::view::*;
use super::*;
use crate::text::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// A unique scratch directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kraal-files-{}-{name}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir.canonicalize().unwrap())
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn mkdir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn text(&self) -> String {
        self.0.to_str().unwrap().to_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn noop() -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(|| {})
}

fn file(name: &str, size: u64) -> Entry {
    Entry { name: name.to_owned(), is_dir: false, is_link: false, size, mtime: 1_700_000_000, exact: true }
}

fn dir(name: &str) -> Entry {
    Entry { name: name.to_owned(), is_dir: true, is_link: false, size: 4096, mtime: 1_700_000_000, exact: true }
}

fn local(path: &str) -> Location {
    Location::Local(PathBuf::from(path))
}

fn remote(path: &str) -> Location {
    let ssh_args = ["-p", "2222", "me@srv"].map(str::to_owned).to_vec();
    Location::Remote(Remote { server: "srv".to_owned(), ssh_args, path: path.to_owned() })
}

fn remote_path(location: &Location) -> &str {
    match location {
        Location::Remote(remote) => &remote.path,
        Location::Local(_) => panic!("not remote: {location:?}"),
    }
}

const LEFT: &str = "/srv/kraal-test/left";
const RIGHT: &str = "/srv/kraal-test/right";

/// Replaces a panel's listing as if it had just arrived, superseding any pending one.
fn inject(pane: &mut FilesPane, side: Side, location: Location, entries: Vec<Entry>) {
    let show_hidden = pane.show_hidden;
    let panel = pane.panel_mut(side);
    panel.supersede();
    panel.apply(Ok((location, entries)), show_hidden);
}

fn inject_error(pane: &mut FilesPane, side: Side, message: &str) {
    let show_hidden = pane.show_hidden;
    let panel = pane.panel_mut(side);
    panel.supersede();
    panel.apply(Err(message.to_owned()), show_hidden);
}

/// Left: two directories, two files and a hidden file; right: empty.
fn sample() -> FilesPane {
    let mut pane = FilesPane::new(local(LEFT), local(RIGHT), noop());
    let entries = vec![file("b.txt", 2048), dir("sub"), file(".hidden", 1), file("A.md", 1024), dir("Docs")];
    inject(&mut pane, Side::Left, local(LEFT), entries);
    inject(&mut pane, Side::Right, local(RIGHT), Vec::new());
    pane
}

/// Names of a panel's rows, `..` included.
fn rows(pane: &FilesPane, side: Side) -> Vec<String> {
    let panel = pane.panel(side);
    (0..panel.rows_len())
        .filter_map(|i| panel.row(i))
        .map(|row| match row {
            Row::Parent => "..".to_owned(),
            Row::Entry(entry) => entry.name.clone(),
        })
        .collect()
}

fn cursor_name(pane: &FilesPane, side: Side) -> String {
    let panel = pane.panel(side);
    rows(pane, side).get(panel.cursor).cloned().unwrap_or_default()
}

fn render(pane: &mut FilesPane, width: u16, height: u16) -> (Buffer, Vec<String>) {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    pane.render(area, &mut buf);
    let text = buffer_rows(&buf, area);
    (buf, text)
}

/// Polls until no panel is loading.
fn settle(pane: &mut FilesPane) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while pane.panels.iter().any(|panel| panel.loading) {
        assert!(Instant::now() < deadline, "listing did not finish: {pane:?}");
        pane.poll();
        thread::sleep(Duration::from_millis(5));
    }
}

// -- Locations ----------------------------------------------------------

#[test]
fn local_locations_join_parent_and_display() {
    let home = local("/home/me");
    assert_eq!(home.join("x y"), local("/home/me/x y"));
    assert_eq!(home.parent(), Some(local("/home")));
    assert_eq!(local("/a").parent(), Some(local("/")));
    assert_eq!(local("/").parent(), None);
    assert_eq!(local("/").join("etc"), local("/etc"));
    assert_eq!(home.display(), "/home/me");
    assert_eq!(home.name().as_deref(), Some("me"));
    assert_eq!(local("/").name(), None);
}

#[test]
fn remote_locations_join_parent_and_display() {
    let log = remote("/var/log");
    assert_eq!(log.display(), "srv:/var/log");
    assert_eq!(log.join("nginx").display(), "srv:/var/log/nginx");
    assert_eq!(log.parent(), Some(remote("/var")));
    assert_eq!(log.name().as_deref(), Some("log"));
    assert_eq!(remote("/var/log/").parent(), Some(remote("/var")));
    assert_eq!(remote("/etc").parent(), Some(remote("/")));
    assert_eq!(remote("/").parent(), None);
    assert_eq!(remote("/").join("etc"), remote("/etc"));
    assert_eq!(remote("/").name(), None);
    // `~` stays symbolic until a listing resolves it.
    assert_eq!(remote("~").display(), "srv:~");
    assert_eq!(remote("~").parent(), None);
    assert_eq!(remote("~").name(), None);
    assert_eq!(remote("~").join("a b"), remote("~/a b"));
    assert_eq!(remote("~/x").parent(), Some(remote("~")));
    assert_eq!(remote("~/x/y").parent(), Some(remote("~/x")));
    assert_eq!(remote("~/x").name().as_deref(), Some("x"));
    assert_eq!(remote("logs").parent(), Some(remote("~")));
    assert_eq!(remote("").join("x"), remote("~/x"));
    // The ssh arguments follow the path around.
    let Location::Remote(child) = log.join("x") else { panic!() };
    assert_eq!(child.ssh_args, ["-p", "2222", "me@srv"]);
    assert_eq!(child.server, "srv");
}

#[test]
fn local_paths_are_resolved() {
    assert_eq!(resolve_local(Path::new("/a/./b/../c/")), PathBuf::from("/a/c"));
    assert_eq!(resolve_local(Path::new("/..")), PathBuf::from("/"));
    if let Some(home) = home_dir() {
        assert_eq!(resolve_local(Path::new("~/x")), resolve_local(&home.join("x")));
    }
    assert!(resolve_local(Path::new("relative")).is_absolute());
}

// -- Remote commands ----------------------------------------------------

#[test]
fn remote_paths_are_quoted() {
    assert_eq!(crate::shell::quote_always("it's"), r"'it'\''s'");
    assert_eq!(remote_dir_arg("~"), "\"$HOME\"");
    assert_eq!(remote_dir_arg(""), "\"$HOME\"");
    assert_eq!(remote_dir_arg("~/a b"), "\"$HOME\"/'a b'");
    assert_eq!(remote_dir_arg("/var/$x"), "'/var/$x'");
    assert_eq!(remote_dir_arg("/it's"), r"'/it'\''s'");
    assert_eq!(remote_dir_arg("~root/x y"), "~root/'x y'");
    assert_eq!(remote_dir_arg("~a b/x"), "'~a b/x'");
    assert_eq!(remote_dir_arg("/é 漢字"), "'/é 漢字'");
}

#[test]
fn quoted_remote_paths_survive_the_shell() {
    let tmp = TempDir::new("quote");
    let names = ["a b", "it's", "$HOME", "`id`", "é 漢字", r"back\slash", "-dash", "new\nline", "tab\there", "\"dq\""];
    for name in names {
        let path = tmp.mkdir(name);
        let absolute = path.to_str().unwrap();
        let cases = [(remote_dir_arg(absolute), "abs"), (remote_dir_arg(&format!("~/{name}")), "home")];
        for (arg, case) in cases.into_iter().chain([(remote_dir_arg(name), "relative")]) {
            let script = format!("cd -- {arg} && printf %s \"$PWD\"");
            let output = Command::new("sh").arg("-c").arg(&script).env("HOME", &tmp.0).current_dir(&tmp.0).output();
            let output = output.unwrap();
            assert_eq!(String::from_utf8_lossy(&output.stdout), absolute, "{case} {name:?}: {script}");
        }
        // The whole command, through the extra `sh -c` layer.
        let output = Command::new("sh").arg("-c").arg(remote_command(absolute)).output().unwrap();
        let (dir, entries) = parse_listing(&output.stdout, 0).expect("listing");
        assert_eq!(dir, absolute);
        assert!(entries.is_empty(), "{entries:?}");
    }
}

#[test]
fn ssh_options_precede_the_destination() {
    let Location::Remote(server) = remote("/var/log") else { panic!() };
    let args = ssh_command_args(&server).unwrap();
    let expected = ["-p", "2222", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-T", "me@srv"];
    assert_eq!(&args[..expected.len()], expected);
    assert_eq!(args.len(), expected.len() + 1);
    assert!(args[expected.len()].starts_with("sh -c 'cd -- '\\''/var/log'\\'' || exit 1;"), "{}", args[8]);
    let empty = Remote { server: "x".to_owned(), ssh_args: Vec::new(), path: "/".to_owned() };
    assert_eq!(ssh_command_args(&empty), None);
}

// -- Parsing ------------------------------------------------------------

#[test]
fn parses_find_output() {
    let mut out = b"motd noise\nKRAAL:LIST\0/srv/my dir\0F\0".to_vec();
    for record in [
        "dd\t4096\t1700000000.5000000000\tsub",
        "ff\t12\t1700000001.0000000000\ta\tb",
        "ff\t3\t1700000002\tline\nbreak",
        "ld\t7\t1700000003.1\tto-dir",
        "lf\t5\t1700000004.2\tto-file",
        "lN\t5\t1700000005\tbroken",
        "ff\t0\t-1.5\tbefore-epoch",
        "ff\t9\t1\té 漢字 '$x",
    ] {
        out.extend_from_slice(record.as_bytes());
        out.push(0);
    }
    out.extend_from_slice(b"ff\t9"); // cut short by a kill
    let (dir, entries) = parse_listing(&out, 0).unwrap();
    assert_eq!(dir, "/srv/my dir");
    let entry = |name: &str, is_dir, is_link, size, mtime| Entry {
        name: name.to_owned(),
        is_dir,
        is_link,
        size,
        mtime,
        exact: true,
    };
    assert_eq!(
        entries,
        [
            entry("sub", true, false, 4096, 1_700_000_000),
            entry("a\tb", false, false, 12, 1_700_000_001),
            entry("line\nbreak", false, false, 3, 1_700_000_002),
            entry("to-dir", true, true, 7, 1_700_000_003),
            entry("to-file", false, true, 5, 1_700_000_004),
            entry("broken", false, true, 5, 1_700_000_005),
            entry("before-epoch", false, false, 0, -1),
            entry("é 漢字 '$x", false, false, 9, 1),
        ]
    );
    assert!(parse_listing(b"no marker", 0).is_err());
    assert!(parse_listing(b"KRAAL:LIST\0/x\0?\0", 0).is_err());
    assert_eq!(parse_listing(b"KRAAL:LIST\0/x\0F\0", 0), Ok(("/x".to_owned(), Vec::new())));
    // A record without its NUL was cut short: `backup` may really be `backup-old`.
    let cut = parse_listing(b"KRAAL:LIST\0/x\0F\0ff\t1\t1\ta\0ff\t12\t1700000000\tbackup", 0).unwrap();
    assert_eq!(cut.1.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a"]);
    // Paths are built on the directory: an approximate one could lead to another.
    let err = parse_listing(b"KRAAL:LIST\0/home/u\xff\0F\0ff\t1\t1\ta\0", 0).unwrap_err();
    assert!(err.contains("not valid UTF-8"), "{err}");
}

#[test]
fn shell_fallback_lists_exact_names() {
    // The fallback branch alone, run here, on names `ls` output would make ambiguous.
    let tmp = TempDir::new("fallback");
    std::fs::create_dir(tmp.0.join("a -> b")).unwrap();
    std::fs::write(tmp.0.join("line\nbreak"), "12345").unwrap();
    std::fs::write(tmp.0.join(".hidden"), "").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("a -> b", tmp.0.join("link")).unwrap();
    let script = remote_script(tmp.0.to_str().unwrap());
    let fallback = script.replace("if find . -maxdepth 0 -printf '' >/dev/null 2>&1", "if false");
    let output = Command::new("sh").arg("-c").arg(&fallback).output().unwrap();
    let now = days_from_civil(2026, 10, 10) * 86_400;
    let (_, mut entries) = parse_listing(&output.stdout, now).expect("listing");
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<(&str, bool, bool)> = entries.iter().map(|e| (e.name.as_str(), e.is_dir, e.is_link)).collect();
    assert_eq!(
        names,
        [(".hidden", false, false), ("a -> b", true, false), ("line\nbreak", false, false), ("link", true, true)]
    );
    let file = entries.iter().find(|e| e.name == "line\nbreak").unwrap();
    assert_eq!(file.size, 5);
    assert!(file.mtime > 0 && entries.iter().all(|e| e.exact));
    let record = b"fd\t1,\tJan  5 12:30\tnull";
    assert_eq!(parse_shell_record(record, now).unwrap().size, 0, "devices have no size");
    let bad = parse_shell_record(b"ff\t1\tJan 5 2024\tbad\xff", now).unwrap();
    assert!(!bad.exact, "a name that is not UTF-8 is marked");
    // Where BSD stat gives the exact time (macOS), it is taken as it is.
    let exact = parse_shell_record(b"ff\t4\t@1791748955\tnotes", now).unwrap();
    assert_eq!((exact.size, exact.mtime), (4, 1791748955));
}

#[test]
fn formats_times_and_sizes() {
    // Dates show in local time; a wall-clock time read as local shows as itself.
    let shown = local_minutes(1_700_000_000);
    assert_eq!((shown.len(), &local_datetime(1_700_000_000)[..16]), (16, shown.as_str()));
    let wall = days_from_civil(2024, 2, 29) * 86_400 + 3_599;
    assert_eq!(local_minutes(wall - utc_offset(wall)), "2024-02-29 00:59");
    for days in [-800_000, -1, 0, 59, 11_016, 2_932_896] {
        let (y, m, d) = civil_from_days(days);
        assert_eq!(days_from_civil(y, m, d), days);
    }
    // Extreme values do not panic.
    let _ = local_minutes(i64::MAX);
    let _ = local_minutes(i64::MIN);
    assert_eq!(human_size(0), "0 B");
    assert_eq!(human_size(1023), "1023 B");
    assert_eq!(human_size(1536), "1.5 KiB");
    assert_eq!(human_size(200 << 20), "200 MiB");
    assert_eq!(human_size(u64::MAX), "16 EiB");
}

// -- Panel logic --------------------------------------------------------

#[test]
fn directories_sort_first_and_hidden_entries_toggle() {
    let mut pane = sample();
    assert_eq!(rows(&pane, Side::Left), ["..", "Docs", "sub", "A.md", "b.txt"]);
    pane.panel_mut(Side::Left).cursor = 4;
    pane.key(FileKey::Char('.'));
    assert_eq!(rows(&pane, Side::Left), ["..", "Docs", "sub", ".hidden", "A.md", "b.txt"]);
    assert_eq!(cursor_name(&pane, Side::Left), "b.txt", "the cursor follows its entry");
    pane.key(FileKey::Char('.'));
    assert_eq!(rows(&pane, Side::Left), ["..", "Docs", "sub", "A.md", "b.txt"]);
    assert_eq!(cursor_name(&pane, Side::Left), "b.txt");
    // The root has no `..`.
    inject(&mut pane, Side::Right, local("/"), vec![file("z", 1), dir("etc")]);
    assert_eq!(rows(&pane, Side::Right), ["etc", "z"]);
}

#[test]
fn marks_toggle_and_move_down() {
    let mut pane = sample();
    pane.key(FileKey::Char(' ')); // on `..`: nothing to mark
    assert!(pane.panel(Side::Left).marked.is_empty());
    assert_eq!(cursor_name(&pane, Side::Left), "Docs");
    pane.key(FileKey::Char(' '));
    pane.key(FileKey::Insert);
    pane.key(FileKey::Char(' '));
    assert_eq!(cursor_name(&pane, Side::Left), "b.txt");
    let names: Vec<&str> = pane.panel(Side::Left).marked_entries().iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Docs", "sub", "A.md"]);
    assert_eq!(pane.panel(Side::Left).status(false).0, "3 marked, 1.0 KiB · 1 hidden");
    pane.key(FileKey::Up);
    pane.key(FileKey::Char(' '));
    assert!(!pane.panel(Side::Left).marked.contains("A.md"), "space unmarks");
    pane.key(FileKey::Char('*'));
    assert_eq!(pane.panel(Side::Left).status(false).0, "4 marked, 3.0 KiB · 1 hidden");
    assert!(!pane.panel(Side::Left).marked.contains(".hidden"), "hidden entries stay unmarked");
    pane.key(FileKey::Char('*'));
    assert!(pane.panel(Side::Left).marked.is_empty());
    assert_eq!(pane.panel(Side::Left).status(true).0, "4 items, 3.0 KiB");
}

#[test]
fn filter_narrows_rows_until_cleared() {
    let mut pane = sample();
    assert_eq!(pane.key(FileKey::Char('/')), FilesAction::None);
    for c in "MD".chars() {
        pane.key(FileKey::Char(c));
    }
    assert_eq!(rows(&pane, Side::Left), ["..", "A.md"]);
    assert_eq!(cursor_name(&pane, Side::Left), "A.md");
    assert_eq!(pane.panel(Side::Left).status(false).0, "/MD▏");
    assert_eq!(pane.hints()[0].label, "type");
    // Letters are typed, not interpreted, while filtering.
    assert_eq!(pane.key(FileKey::Char('q')), FilesAction::None);
    assert_eq!(rows(&pane, Side::Left), [".."]);
    pane.key(FileKey::Backspace);
    assert_eq!(rows(&pane, Side::Left), ["..", "A.md"]);
    // Enter keeps the filter.
    pane.key(FileKey::Enter);
    assert!(!pane.panel(Side::Left).editing);
    assert_eq!(rows(&pane, Side::Left), ["..", "A.md"]);
    assert_eq!(pane.panel(Side::Left).status(false).0, "1 item, 1.0 KiB · filter: MD · 1 hidden");
    // Esc first clears the filter, then closes.
    assert_eq!(pane.key(FileKey::Esc), FilesAction::None);
    assert_eq!(rows(&pane, Side::Left), ["..", "Docs", "sub", "A.md", "b.txt"]);
    assert_eq!(cursor_name(&pane, Side::Left), "A.md");
    assert_eq!(pane.key(FileKey::Esc), FilesAction::Close);
    // Esc while typing clears too.
    pane.key(FileKey::Char('/'));
    pane.key(FileKey::Char('s'));
    assert_eq!(rows(&pane, Side::Left), ["..", "Docs", "sub"]);
    pane.key(FileKey::Esc);
    assert_eq!(rows(&pane, Side::Left), ["..", "Docs", "sub", "A.md", "b.txt"]);
    assert!(!pane.panel(Side::Left).editing);
}

#[test]
fn refresh_keeps_cursor_and_marks() {
    let mut pane = sample();
    for _ in 0..3 {
        pane.key(FileKey::Down);
    }
    pane.key(FileKey::Char(' ')); // marks A.md, cursor on b.txt
    let listing = vec![file("0new", 1), file("A.md", 1), file("b.txt", 1), dir("Docs")];
    inject(&mut pane, Side::Left, local(LEFT), listing);
    assert_eq!(rows(&pane, Side::Left), ["..", "Docs", "0new", "A.md", "b.txt"]);
    assert_eq!(cursor_name(&pane, Side::Left), "b.txt");
    assert!(pane.panel(Side::Left).marked.contains("A.md"));
    inject(&mut pane, Side::Left, local(LEFT), vec![file("A.md", 1)]);
    assert_eq!(cursor_name(&pane, Side::Left), "A.md", "clamped to the last row");
    assert!(pane.panel(Side::Left).marked.contains("A.md"));
    inject(&mut pane, Side::Left, local(LEFT), Vec::new());
    assert!(pane.panel(Side::Left).marked.is_empty());
    assert_eq!(pane.panel(Side::Left).cursor, 0);
}

#[test]
fn going_up_selects_the_directory_left() {
    let mut pane = sample();
    inject(&mut pane, Side::Left, local("/srv/kraal-test/left/sub"), vec![file("x", 1)]);
    pane.key(FileKey::Backspace);
    assert_eq!(pane.location(Side::Left), &local(LEFT));
    assert!(pane.panel(Side::Left).loading);
    inject(&mut pane, Side::Left, local(LEFT), vec![dir("a"), dir("sub"), dir("z")]);
    assert_eq!(cursor_name(&pane, Side::Left), "sub");
    // Enter on `..` does the same.
    inject(&mut pane, Side::Left, local("/srv/kraal-test/left/z"), Vec::new());
    assert_eq!(cursor_name(&pane, Side::Left), "..");
    pane.key(FileKey::Enter);
    inject(&mut pane, Side::Left, local(LEFT), vec![dir("a"), dir("sub"), dir("z")]);
    assert_eq!(cursor_name(&pane, Side::Left), "z");
    // Remote, before and after `~` is resolved.
    pane.set_location(Side::Right, remote("~/x"));
    pane.active = Side::Right;
    pane.key(FileKey::Backspace);
    assert_eq!(pane.location(Side::Right), &remote("~"));
    inject(&mut pane, Side::Right, remote("/home/me"), vec![dir("w"), dir("x")]);
    assert_eq!(cursor_name(&pane, Side::Right), "x");
    assert_eq!(rows(&pane, Side::Right)[0], "..", "a resolved home has a parent");
    pane.key(FileKey::Backspace);
    assert_eq!(pane.location(Side::Right), &remote("/home"));
}

#[test]
fn copies_say_what_they_would_replace() {
    let mut pane = sample();
    inject(&mut pane, Side::Right, local(RIGHT), vec![file("A.md", 10), file("other", 1)]);
    pane.key(FileKey::End);
    pane.key(FileKey::Char(' '));
    pane.key(FileKey::Up);
    pane.key(FileKey::Char(' '));
    let FilesAction::Copy { replaces, .. } = pane.key(FileKey::Char('c')) else { panic!("a copy") };
    assert_eq!(replaces, vec!["A.md".to_string()], "b.txt is new there, A.md is not");
}

#[test]
fn keys_map_to_actions() {
    let mut pane = sample();
    // Nothing acts on `..`.
    for key in [FileKey::Char('c'), FileKey::Char('m'), FileKey::Char('d'), FileKey::F(5), FileKey::Delete] {
        assert_eq!(pane.key(key), FilesAction::None, "{key:?}");
    }
    pane.key(FileKey::Down);
    let docs = local("/srv/kraal-test/left/Docs");
    let copy = FilesAction::Copy { sources: vec![docs.clone()], dest: local(RIGHT), replaces: vec![] };
    assert_eq!(pane.key(FileKey::Char('c')), copy);
    assert_eq!(pane.key(FileKey::F(5)), copy);
    assert_eq!(
        pane.key(FileKey::F(6)),
        FilesAction::Move { sources: vec![docs.clone()], dest: local(RIGHT), replaces: vec![] }
    );
    assert_eq!(pane.key(FileKey::F(8)), FilesAction::Delete(vec![docs.clone()]));
    // Marks win over the cursor, in display order; a copy clears them.
    pane.key(FileKey::End);
    pane.key(FileKey::Char(' '));
    pane.key(FileKey::Up);
    pane.key(FileKey::Char(' '));
    let marked = vec![local("/srv/kraal-test/left/A.md"), local("/srv/kraal-test/left/b.txt")];
    assert_eq!(
        pane.key(FileKey::Char('m')),
        FilesAction::Move { sources: marked.clone(), dest: local(RIGHT), replaces: vec![] }
    );
    assert_eq!(pane.key(FileKey::Char('d')), FilesAction::Delete(marked.clone()));
    assert_eq!(pane.key(FileKey::Delete), FilesAction::Delete(marked.clone()));
    assert_eq!(
        pane.key(FileKey::Char('c')),
        FilesAction::Copy { sources: marked, dest: local(RIGHT), replaces: vec![] }
    );
    assert!(pane.panel(Side::Left).marked.is_empty());
    assert_eq!(pane.key(FileKey::Char('s')), FilesAction::Shell(local(LEFT)));
    assert_eq!(pane.key(FileKey::Char('o')), FilesAction::PickLocation(Side::Left));
    assert_eq!(pane.key(FileKey::Char(':')), FilesAction::PickLocation(Side::Left));
    // Enter or F3 on a local file previews it.
    assert_eq!(cursor_name(&pane, Side::Left), "b.txt");
    let preview = FilesAction::Preview(PathBuf::from("/srv/kraal-test/left/b.txt"));
    assert_eq!(pane.key(FileKey::Enter), preview);
    assert_eq!(pane.key(FileKey::F(3)), preview);
    // Tab switches panels; the right one is empty.
    assert_eq!(pane.key(FileKey::Tab), FilesAction::None);
    assert_eq!(pane.active(), Side::Right);
    assert_eq!(pane.hints()[4].text, "copy ←");
    assert_eq!(pane.key(FileKey::Char('c')), FilesAction::None);
    assert_eq!(pane.key(FileKey::Char('o')), FilesAction::PickLocation(Side::Right));
    pane.key(FileKey::Tab);
    assert_eq!(pane.active(), Side::Left);
    for key in [FileKey::Char('q'), FileKey::Esc, FileKey::F(10)] {
        assert_eq!(pane.key(key), FilesAction::Close, "{key:?}");
    }
    // No copy into the directory itself.
    inject(&mut pane, Side::Right, local(LEFT), Vec::new());
    assert_eq!(pane.key(FileKey::Char('c')), FilesAction::None);
    assert_eq!(pane.key(FileKey::Char('d')), FilesAction::Delete(vec![local("/srv/kraal-test/left/b.txt")]));
    // Enter on a directory opens it.
    pane.key(FileKey::Home);
    pane.key(FileKey::Down);
    assert_eq!(pane.key(FileKey::Enter), FilesAction::None);
    assert_eq!(pane.location(Side::Left), &docs);
    assert!(pane.panel(Side::Left).loading);
}

#[test]
fn remote_entries_are_copied_but_not_previewed() {
    let mut pane = FilesPane::new(remote("/var/log"), local(RIGHT), noop());
    let mut link = dir("current");
    link.is_link = true;
    inject(&mut pane, Side::Left, remote("/var/log"), vec![file("syslog", 10), link]);
    assert_eq!(rows(&pane, Side::Left), ["..", "current", "syslog"]);
    pane.key(FileKey::End);
    assert_eq!(pane.key(FileKey::Enter), FilesAction::None);
    assert_eq!(pane.location(Side::Left), &remote("/var/log"));
    let sources = vec![remote("/var/log/syslog")];
    assert_eq!(pane.key(FileKey::Char('c')), FilesAction::Copy { sources, dest: local(RIGHT), replaces: vec![] });
    assert_eq!(pane.key(FileKey::Char('s')), FilesAction::Shell(remote("/var/log")));
    // Links to directories are entered.
    pane.key(FileKey::Up);
    pane.key(FileKey::Enter);
    assert_eq!(pane.location(Side::Left), &remote("/var/log/current"));
    assert_eq!(pane.title(), format!("files · srv:/var/log/current ⇄ {RIGHT}"));
}

#[test]
fn navigation_keys_move_the_cursor() {
    let mut pane = sample();
    let many = (0..100).map(|i| file(&format!("f{i:03}"), i)).collect();
    inject(&mut pane, Side::Left, local(LEFT), many);
    render(&mut pane, 60, 12); // 8 listing rows
    pane.key(FileKey::Up);
    assert_eq!(pane.panel(Side::Left).cursor, 0);
    pane.key(FileKey::PageDown);
    assert_eq!(pane.panel(Side::Left).cursor, 7);
    pane.key(FileKey::Char('j'));
    pane.key(FileKey::Down);
    assert_eq!(pane.panel(Side::Left).cursor, 9);
    pane.key(FileKey::Char('k'));
    assert_eq!(pane.panel(Side::Left).cursor, 8);
    pane.key(FileKey::End);
    assert_eq!(cursor_name(&pane, Side::Left), "f099");
    pane.key(FileKey::Down);
    assert_eq!(cursor_name(&pane, Side::Left), "f099");
    let (_, text) = render(&mut pane, 60, 12);
    assert!(text[9].contains("f099"), "{text:#?}");
    pane.key(FileKey::PageUp);
    assert_eq!(cursor_name(&pane, Side::Left), "f092");
    pane.key(FileKey::Home);
    assert_eq!(cursor_name(&pane, Side::Left), "..");
    let (_, text) = render(&mut pane, 60, 12);
    assert!(text[2].contains(".."), "{text:#?}");
}

// -- Rendering ----------------------------------------------------------

#[test]
fn renders_headers_listing_and_footer() {
    let mut pane = sample();
    assert_eq!(pane.title(), format!("files · {LEFT} ⇄ {RIGHT}"));
    let (buf, text) = render(&mut pane, 80, 12);
    assert!(text[0].starts_with(&format!("▸ {LEFT}")), "the active panel is marked: {text:#?}");
    assert!(text[0].ends_with(&format!("│  {RIGHT}")), "{text:#?}");
    let t = crate::theme::current();
    assert_eq!(buf[(0, 0)].bg, t.accent, "the active header is highlighted");
    assert_eq!(buf[(45, 0)].bg, Color::Indexed(236));
    assert_eq!(buf[(39, 5)].symbol(), "│");
    assert!(text[1].starts_with(" Name") && text[1].contains("Size") && text[1].contains("Modified"));
    assert!(text[2].starts_with("›..") && text[2].contains("UP--DIR"), "the cursor is marked: {text:#?}");
    assert_eq!(buf[(1, 2)].bg, t.selection, "cursor on the active panel");
    let when = local_minutes(1_700_000_000);
    assert!(text[3].starts_with(" Docs/") && text[3].contains(&format!("<DIR> {when}")), "{text:#?}");
    assert_eq!(buf[(1, 3)].fg, Color::LightBlue);
    assert!(text[5].starts_with(" A.md") && text[5].contains(&format!("1.0 KiB {when}")), "{text:#?}");
    assert!(text[2][..39].ends_with(' '), "the left panel stays left of the separator");
    assert!(text[3].ends_with("│ (empty)"), "{text:#?}");
    assert!(text[10].starts_with(" 4 items, 3.0 KiB · 1 hidden"), "{text:#?}");
    assert!(text[10].ends_with("│ 0 items, 0 B"), "{text:#?}");
    let footer = " tab switch · enter open · ⌫ up · space mark · c copy → · m move · d delete";
    assert_eq!(text[11], footer, "hints that do not fit are left out");
    let (_, text) = render(&mut pane, 160, 12);
    assert!(text[11].ends_with("r refresh · . hidden · / filter · q close"), "{text:#?}");
    // Marked entries are yellow.
    pane.key(FileKey::Down);
    pane.key(FileKey::Char(' '));
    let (buf, text) = render(&mut pane, 80, 12);
    assert_eq!(buf[(1, 3)].fg, t.blocked);
    assert!(text[10].starts_with(" 1 marked · 1 hidden"), "only a directory is marked: {text:#?}");
    assert_eq!(pane.visible_text(), text.join("\n"));
    // The inactive panel's cursor is subdued.
    pane.key(FileKey::Tab);
    let (buf, _) = render(&mut pane, 80, 12);
    assert_eq!(buf[(45, 0)].bg, t.accent);
    assert_eq!(buf[(1, 4)].bg, Color::Indexed(235));
}

#[test]
fn narrow_areas_show_the_active_panel() {
    let mut pane = sample();
    let (_, text) = render(&mut pane, 20, 6);
    assert!(text[0].starts_with("▸ …"), "the header keeps the end of the path: {text:#?}");
    assert!(text[0].ends_with("left"), "{text:#?}");
    assert!(text[1].starts_with("›.."), "{text:#?}");
    pane.key(FileKey::Tab);
    let (_, text) = render(&mut pane, 20, 6);
    assert!(text[0].ends_with("right"), "{text:#?}");
    assert!(text[2].starts_with(" (empty)"), "{text:#?}");
}

#[test]
fn renders_loading_and_error_states() {
    let mut pane = FilesPane::new(local(LEFT), remote("/var/log"), noop());
    let (_, text) = render(&mut pane, 80, 12);
    assert!(text[3].starts_with(" loading…") && text[3].contains("│ loading…"), "{text:#?}");
    // A refresh keeps the old entries and says so in the header.
    inject(&mut pane, Side::Left, local(LEFT), vec![file("a", 1)]);
    pane.refresh();
    let (_, text) = render(&mut pane, 80, 12);
    assert!(text[0].starts_with(&format!("▸ {LEFT}  loading…")), "{text:#?}");
    assert!(text[3].starts_with(" a "), "{text:#?}");
    // Errors are red, below `..`; the location stays for a retry.
    let message = "ssh: connect to host srv port 22: Connection refused";
    inject_error(&mut pane, Side::Right, message);
    let (buf, text) = render(&mut pane, 80, 12);
    assert!(text[2].contains("│ .."), "{text:#?}");
    assert!(text[3].ends_with("│ ssh: connect to host srv port 22:"), "{text:#?}");
    assert!(text[4].ends_with("│ Connection refused"), "{text:#?}");
    assert!(text[10].ends_with("│"), "no status for a failed panel: {text:#?}");
    assert_eq!(buf[(41, 3)].fg, Color::Red);
    assert_eq!(pane.location(Side::Right), &remote("/var/log"));
    pane.key(FileKey::Tab);
    pane.key(FileKey::Char('r'));
    assert!(pane.panel(Side::Right).loading);
    assert_eq!(pane.panel(Side::Right).error.as_deref(), Some(message));
}

#[test]
fn tiny_areas_never_panic_or_overflow() {
    let mut pane = sample();
    let odd = vec![file("漢字漢字漢字漢字漢字.txt", 1), file("ctl\x1b[31m\nname", u64::MAX), dir("é")];
    inject(&mut pane, Side::Right, local("/"), odd);
    inject_error(&mut pane, Side::Left, "boom\nwith\tcontrol\x07 chars and a long long long message");
    let outer = Rect::new(0, 0, 40, 14);
    for width in 0..=34 {
        for height in 0..=11 {
            let mut buf = Buffer::empty(outer);
            for cell in &mut buf.content {
                cell.set_symbol("X");
            }
            let area = Rect::new(3, 2, width, height);
            pane.render(area, &mut buf);
            for y in outer.top()..outer.bottom() {
                for x in outer.left()..outer.right() {
                    if !area.contains((x, y).into()) {
                        assert_eq!(buf[(x, y)].symbol(), "X", "{width}x{height} wrote at ({x}, {y})");
                    }
                }
            }
            let _ = pane.visible_text();
            for (col, row) in [(0, 0), (width / 2, height / 2), (width.saturating_sub(1), height.saturating_sub(1))] {
                assert_ne!(pane.click(col, row, false), FilesAction::Close);
                pane.scroll(col, 3);
                pane.scroll(col, -3);
            }
            pane.key(FileKey::PageDown);
            pane.key(FileKey::Tab);
        }
    }
    // An area reaching past the buffer is clipped.
    let mut buf = Buffer::empty(outer);
    pane.render(Rect::new(30, 10, 30, 30), &mut buf);
    pane.render(Rect::new(100, 100, 10, 10), &mut buf);
    assert_eq!(pane.visible_text(), "");
}

#[test]
fn mouse_selects_panels_and_entries() {
    let mut pane = sample();
    render(&mut pane, 80, 12);
    assert_eq!(pane.click(50, 3, false), FilesAction::None);
    assert_eq!(pane.active(), Side::Right);
    assert_eq!(pane.click(5, 4, false), FilesAction::None);
    assert_eq!(pane.active(), Side::Left);
    assert_eq!(cursor_name(&pane, Side::Left), "sub");
    assert_eq!(pane.click(5, 9, false), FilesAction::None, "below the last row");
    assert_eq!(cursor_name(&pane, Side::Left), "sub");
    let preview = FilesAction::Preview(PathBuf::from("/srv/kraal-test/left/b.txt"));
    assert_eq!(pane.click(5, 6, true), preview);
    assert_eq!(pane.click(39, 6, true), FilesAction::None, "the separator");
    // Hints are buttons.
    assert_eq!(pane.click(2, 11, false), FilesAction::None);
    assert_eq!(pane.active(), Side::Right);
    let hints = pane.hints();
    let (start, _, _) = footer_layout(&hints, 80).into_iter().find(|&(_, _, i)| hints[i].label == "d").unwrap();
    assert_eq!(pane.click(start, 11, false), FilesAction::None, "nothing to delete on the right");
    assert_eq!(pane.click(79, 11, false), FilesAction::None, "past the last hint");
    render(&mut pane, 160, 12);
    let (start, end, _) = footer_layout(&hints, 160).into_iter().find(|&(_, _, i)| hints[i].label == "o").unwrap();
    assert_eq!(pane.click(end - 1, 11, false), FilesAction::PickLocation(Side::Right));
    assert_eq!(pane.click(start - 2, 11, false), FilesAction::None, "between hints");
    // Double click on a directory opens it.
    assert_eq!(pane.click(5, 3, true), FilesAction::None);
    assert_eq!(pane.location(Side::Left), &local("/srv/kraal-test/left/Docs"));
}

#[test]
fn wheel_scrolls_the_panel_under_the_mouse() {
    let mut pane = sample();
    let many = (0..50).map(|i| file(&format!("f{i:02}"), 1)).collect();
    inject(&mut pane, Side::Right, local(RIGHT), many);
    render(&mut pane, 80, 12); // 8 listing rows
    pane.scroll(60, 5);
    assert_eq!(pane.active(), Side::Left, "the wheel does not change the active panel");
    assert_eq!(pane.panel(Side::Right).scroll_offset(0, 8), 5);
    assert_eq!(pane.panel(Side::Right).cursor, 5, "the cursor stays visible");
    pane.scroll(60, 1000);
    assert_eq!(pane.panel(Side::Right).scroll_offset(0, 8), 51 - 8);
    let (_, text) = render(&mut pane, 80, 12);
    assert!(text[9].ends_with("│ f49") || text[9].contains("│ f49 "), "{text:#?}");
    pane.scroll(60, -1000);
    assert_eq!(pane.panel(Side::Right).scroll_offset(0, 8), 0);
    assert_eq!(pane.panel(Side::Right).cursor, 7);
}

#[test]
fn clicks_count_in_what_their_client_shows() {
    let mut pane = sample();
    let many = (0..50).map(|i| file(&format!("f{i:02}"), 1)).collect();
    inject(&mut pane, Side::Left, local(LEFT), many);
    // Client 1, tall (8 listing rows), takes the cursor down, then back up to the top of what it
    // shows: its view starts where no other size would start it…
    pane.set_viewer(1);
    render(&mut pane, 80, 12);
    for _ in 0..30 {
        pane.key(FileKey::Down);
    }
    pane.set_viewer(1);
    render(&mut pane, 80, 12);
    for _ in 0..7 {
        pane.key(FileKey::Up);
    }
    pane.set_viewer(1);
    let (_, tall) = render(&mut pane, 80, 12);
    // …then other clients draw the same pane, at many sizes (one resizing its terminal row by
    // row, many others of their own, more than any small limit): each scrolls its own view.
    for rows in 5..40 {
        pane.set_viewer(2);
        render(&mut pane, 80, rows);
    }
    for viewer in 3..80 {
        pane.set_viewer(viewer);
        render(&mut pane, 80, 6);
    }
    // A click in client 1's view still picks the row it showed there.
    pane.set_view(1, 80, 12);
    let row = 4;
    let shown = tall[usize::from(row)].clone();
    pane.click(5, row, false);
    let picked = pane.panel(Side::Left).current().map(|e| e.name.clone()).unwrap_or_default();
    assert!(shown.contains(&picked), "clicked {shown:?}, picked {picked:?}");
    // A client that detaches leaves nothing behind.
    pane.forget_viewer(1);
    assert!(pane.panels.iter().all(|p| !p.offsets.contains_key(&1)));
    assert!(pane.panels[0].offsets.contains_key(&79));
}

// -- Background listings ------------------------------------------------

#[test]
fn lists_a_local_directory_in_the_background() {
    let tmp = TempDir::new("local");
    tmp.write("a.txt", "hello");
    tmp.write("b c.txt", "");
    tmp.write(".hidden", "");
    tmp.mkdir("sub");
    tmp.write("sub/inner", "x");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(tmp.0.join("sub"), tmp.0.join("link")).unwrap();
        std::os::unix::fs::symlink(tmp.0.join("nowhere"), tmp.0.join("dangling")).unwrap();
    }
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&wakes);
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    let mut pane = FilesPane::new(Location::Local(tmp.0.clone()), Location::Local(tmp.0.join("sub")), wake);
    assert!(pane.visible_text().contains("loading…"));
    settle(&mut pane);
    assert!(wakes.load(Ordering::Relaxed) >= 2);
    assert!(!pane.poll(), "nothing left to apply");
    let expected: &[&str] = if cfg!(unix) {
        &["..", "link", "sub", "a.txt", "b c.txt", "dangling"]
    } else {
        &["..", "sub", "a.txt", "b c.txt"]
    };
    assert_eq!(rows(&pane, Side::Left), expected);
    assert_eq!(rows(&pane, Side::Right), ["..", "inner"]);
    let entries = &pane.panel(Side::Left).entries;
    let find = |name: &str| entries.iter().find(|e| e.name == name).unwrap();
    assert_eq!(find("a.txt").size, 5);
    assert!((find("a.txt").mtime - unix_seconds(SystemTime::now())).abs() < 3600);
    if cfg!(unix) {
        assert!(find("link").is_dir && find("link").is_link);
        assert!(!find("dangling").is_dir && find("dangling").is_link);
    }
    let (_, text) = render(&mut pane, 100, 12);
    assert!(text.iter().any(|row| row.starts_with(" a.txt") && row.contains("5 B")), "{text:#?}");
    // Into `sub` and back up.
    pane.key(FileKey::Char('/'));
    pane.key(FileKey::Char('u'));
    pane.key(FileKey::Char('b'));
    pane.key(FileKey::Enter);
    assert_eq!(cursor_name(&pane, Side::Left), "sub");
    pane.key(FileKey::Enter);
    settle(&mut pane);
    assert_eq!(pane.location(Side::Left), &Location::Local(tmp.0.join("sub")));
    assert_eq!(rows(&pane, Side::Left), ["..", "inner"]);
    pane.key(FileKey::Backspace);
    settle(&mut pane);
    assert_eq!(pane.location(Side::Left), &Location::Local(tmp.0.clone()));
    assert_eq!(cursor_name(&pane, Side::Left), "sub");
    // Errors leave the location for a retry.
    pane.set_location(Side::Right, Location::Local(tmp.0.join("missing")));
    settle(&mut pane);
    let error = pane.panel(Side::Right).error.clone().unwrap();
    assert!(error.starts_with("cannot list "), "{error}");
    tmp.mkdir("missing");
    pane.refresh();
    settle(&mut pane);
    assert_eq!(pane.panel(Side::Right).error, None);
}

#[test]
fn a_newer_listing_supersedes_an_older_one() {
    let one = TempDir::new("first");
    let two = TempDir::new("second");
    one.write("one", "");
    two.write("two", "");
    let mut pane = FilesPane::new(Location::Local(one.0.clone()), Location::Local(one.0.clone()), noop());
    pane.set_location(Side::Left, Location::Local(two.0.clone()));
    settle(&mut pane);
    // Wait for the first listing's thread too: its result must be dropped.
    thread::sleep(Duration::from_millis(100));
    pane.poll();
    assert_eq!(rows(&pane, Side::Left), ["..", "two"]);
    assert_eq!(rows(&pane, Side::Right), ["..", "one"]);
}

/// Writes a fake ssh, run as `sh <script> <ssh args…>`: it checks that
/// BatchMode precedes the destination, then runs the remote command
/// locally with the destination as home directory.
fn fake_ssh(tmp: &TempDir, setup: &str) -> String {
    let script = format!(
        r#"dest=; cmd=; batch=0; i=0
for arg; do
  i=$((i + 1))
  [ "$arg" = BatchMode=yes ] && batch=$i
  [ $i -eq $(($# - 1)) ] && dest=$arg
  [ $i -eq $# ] && cmd=$arg
done
if [ "$batch" -eq 0 ] || [ "$batch" -ge $(($# - 1)) ]; then
  echo 'fake ssh: BatchMode must precede the destination' >&2; exit 255
fi
HOME=$dest; export HOME
cd "$HOME" || exit 255
{setup}
exec sh -c "$cmd"
"#
    );
    tmp.write("fake-ssh.sh", &script).to_str().unwrap().to_owned()
}

fn fake_remote(script: &str, home: &TempDir, path: &str) -> Location {
    let ssh_args = vec![script.to_owned(), "-p".to_owned(), "2222".to_owned(), home.text()];
    Location::Remote(Remote { server: "fake".to_owned(), ssh_args, path: path.to_owned() })
}

fn fake_pane(left: Location, right: Location, timeout: Duration) -> FilesPane {
    FilesPane::with_ssh(left, right, noop(), Ssh { program: "sh".to_owned(), timeout })
}

#[cfg(unix)]
#[test]
fn lists_a_remote_directory_through_ssh() {
    let tools = TempDir::new("ssh");
    let home = TempDir::new("home");
    let script = fake_ssh(&tools, "");
    for name in ["sp ace", "quo'te", "dol$lar", "é 漢字", "tab\tname", "new\nline", r"back\slash", ".dot"] {
        home.write(name, "1234");
    }
    let inner = home.mkdir("dir it's $HOME");
    fs::write(inner.join("inner file"), "").unwrap();
    std::os::unix::fs::symlink(&inner, home.0.join("ln")).unwrap();
    let mut pane = fake_pane(fake_remote(&script, &home, "~"), Location::Local(home.0.clone()), SSH_TIMEOUT);
    settle(&mut pane);
    assert_eq!(pane.panel(Side::Left).error, None);
    assert_eq!(remote_path(pane.location(Side::Left)), home.text(), "`~` is resolved");
    // Same entries as the local listing of the same directory.
    assert_eq!(pane.panel(Side::Left).entries.len(), 10);
    assert_eq!(pane.panel(Side::Left).entries, pane.panel(Side::Right).entries);
    assert_eq!(rows(&pane, Side::Left), rows(&pane, Side::Right));
    let ln = pane.panel(Side::Left).entries.iter().find(|e| e.name == "ln").unwrap();
    assert!(ln.is_dir && ln.is_link);
    // Into the directory with an awkward name, and back.
    let row = pane.panel(Side::Left).row_of("dir it's $HOME").unwrap();
    pane.panel_mut(Side::Left).cursor = row;
    pane.key(FileKey::Enter);
    settle(&mut pane);
    assert_eq!(pane.panel(Side::Left).error, None);
    assert_eq!(remote_path(pane.location(Side::Left)), format!("{}/dir it's $HOME", home.text()));
    assert_eq!(rows(&pane, Side::Left), ["..", "inner file"]);
    pane.key(FileKey::Backspace);
    settle(&mut pane);
    assert_eq!(remote_path(pane.location(Side::Left)), home.text());
    assert_eq!(cursor_name(&pane, Side::Left), "dir it's $HOME");
    // A relative path is relative to the home directory.
    pane.set_location(Side::Left, fake_remote(&script, &home, "dir it's $HOME"));
    settle(&mut pane);
    assert_eq!(remote_path(pane.location(Side::Left)), format!("{}/dir it's $HOME", home.text()));
}

#[cfg(unix)]
#[test]
fn remote_listing_falls_back_to_ls() {
    let tools = TempDir::new("ls-tools");
    let home = TempDir::new("ls-home");
    // A PATH without find: only sh and ls, as symlinks.
    let bin = tools.mkdir("bin");
    for tool in ["sh", "ls"] {
        let found = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join(tool))
            .find(|path| path.is_file())
            .unwrap();
        std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
    }
    let script = fake_ssh(&tools, &format!("PATH={}; export PATH", crate::shell::quote_always(bin.to_str().unwrap())));
    home.write("sp ace", "12345");
    home.write("quo'te", "");
    home.mkdir("d");
    std::os::unix::fs::symlink(home.0.join("d"), home.0.join("ln")).unwrap();
    std::os::unix::fs::symlink(home.0.join("sp ace"), home.0.join("fl")).unwrap();
    let mut pane = fake_pane(fake_remote(&script, &home, "~"), Location::Local(home.0.clone()), SSH_TIMEOUT);
    settle(&mut pane);
    assert_eq!(pane.panel(Side::Left).error, None);
    assert_eq!(rows(&pane, Side::Left), ["..", "d", "ln", "fl", "quo'te", "sp ace"]);
    let entries = &pane.panel(Side::Left).entries;
    let find = |name: &str| entries.iter().find(|e| e.name == name).unwrap();
    assert!(find("ln").is_dir && find("ln").is_link);
    assert!(!find("fl").is_dir && find("fl").is_link);
    assert_eq!(find("sp ace").size, 5);
    // ls shows local time without seconds, read as UTC.
    assert!((find("sp ace").mtime - unix_seconds(SystemTime::now())).abs() < 2 * 86_400);
}

#[cfg(unix)]
#[test]
fn ssh_failures_and_timeouts_are_shown() {
    let tools = TempDir::new("fail");
    let home = TempDir::new("fail-home");
    let refused =
        tools.write("refused.sh", "echo 'ssh: connect to host srv port 22: Connection refused' >&2\nexit 255\n");
    let hang = tools.write("hang.sh", "exec sleep 30\n");
    let refused = fake_remote(refused.to_str().unwrap(), &home, "/x");
    let hang = fake_remote(hang.to_str().unwrap(), &home, "~");
    let started = Instant::now();
    let mut pane = fake_pane(refused, hang, Duration::from_millis(300));
    settle(&mut pane);
    assert!(started.elapsed() < Duration::from_secs(10));
    let error = |pane: &FilesPane, side: Side| pane.panel(side).error.clone().unwrap_or_default();
    assert_eq!(error(&pane, Side::Left), "ssh: connect to host srv port 22: Connection refused");
    assert_eq!(error(&pane, Side::Right), "timed out after 300ms");
    assert_eq!(remote_path(pane.location(Side::Right)), "~", "the location stays for a retry");
    let (buf, text) = render(&mut pane, 100, 12);
    assert!(text[3].starts_with(" ssh: connect to host srv port 22:"), "{text:#?}");
    assert_eq!(buf[(1, 3)].fg, Color::Red);
    // A missing destination or program is an error too.
    let mut pane = FilesPane::new(remote("/"), remote("/"), noop());
    let Location::Remote(mut lost) = remote("/") else { panic!() };
    lost.ssh_args.clear();
    pane.set_location(Side::Right, Location::Remote(lost));
    settle(&mut pane);
    assert!(error(&pane, Side::Left).starts_with("cannot run kraal-test-no-ssh"), "{}", error(&pane, Side::Left));
    assert_eq!(error(&pane, Side::Right), "no ssh destination for srv");
}
