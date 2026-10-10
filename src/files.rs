//! A two-panel file manager pane in the style of Midnight Commander.
//!
//! Each panel shows a local directory or a directory on a server reached over
//! SSH. Listings run on background threads and are applied by
//! [`FilesPane::poll`], so the pane never blocks the server. File operations
//! are not performed here: keys return [`FilesAction`]s that the server
//! carries out (with `kraal cp` semantics) before calling
//! [`FilesPane::refresh`]. Errors are rendered inside the panels; nothing
//! panics or draws outside the given area, whatever the input or size.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Read;
use std::iter;
use std::mem;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

/// How long a remote listing may take before ssh is killed.
const SSH_TIMEOUT: Duration = Duration::from_secs(15);
/// Options inserted before the destination of every listing ssh.
const SSH_OPTIONS: [&str; 5] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-T"];
/// Program used for remote listings. Tests use a name that cannot exist, so
/// that nothing ever connects anywhere unless a test installs a fake.
#[cfg(not(test))]
const SSH_PROGRAM: &str = "ssh";
#[cfg(test)]
const SSH_PROGRAM: &str = "kraal-test-no-ssh";
/// How often a running ssh is checked for exit, timeout or cancellation.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Precedes the remote listing, so that noise printed by login scripts is skipped.
const MARKER: &[u8] = b"KRAAL:LIST\0";
/// Most bytes of ssh's stderr kept for an error message.
const MAX_STDERR: usize = 16 << 10;
/// Viewport assumed before the first render.
const DEFAULT_VIEW: (u16, u16) = (80, 24);
/// Narrower areas show only the active panel.
const MIN_SPLIT_WIDTH: u16 = 24;
const SIZE_WIDTH: usize = 7;
const DATE_WIDTH: usize = 16;
/// Narrowest name column kept before the date, then the size, are dropped.
const MIN_NAME_WIDTH: usize = 8;

const DIM: Style = Style::new().fg(Color::DarkGray);
const NOTE: Style = DIM.add_modifier(Modifier::ITALIC);
const ERROR: Style = Style::new().fg(Color::Red);
const DIR_NAME: Style = Style::new().fg(Color::LightBlue).add_modifier(Modifier::BOLD);
const LINK_NAME: Style = Style::new().fg(Color::LightCyan);
const MARKED: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
const FILTER: Style = Style::new().fg(Color::Yellow);
const HEADER_ACTIVE: Style = Style::new().fg(Color::Black).bg(Color::LightBlue).add_modifier(Modifier::BOLD);
const HEADER_INACTIVE: Style = Style::new().fg(Color::Gray).bg(Color::Indexed(237));
const CURSOR_ACTIVE: Style = Style::new().fg(Color::Black).bg(Color::Cyan);
const CURSOR_INACTIVE: Style = Style::new().bg(Color::Indexed(238));
const HINT_KEY: Style = Style::new().add_modifier(Modifier::BOLD);
const HINT_TEXT: Style = DIM;

/// Where a panel points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A directory on this machine.
    Local(PathBuf),
    /// A directory on a server, listed over SSH.
    Remote(Remote),
}

/// A directory on a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// Display name of the server (e.g. "edge-proxy").
    pub server: String,
    /// Arguments for `ssh` that reach the server, destination last,
    /// e.g. `["-p", "2222", "-o", "ControlPath=…", "user@host"]`.
    pub ssh_args: Vec<String>,
    /// Absolute path, or "~" / "~/x" before the first listing resolves it.
    pub path: String,
}

impl Location {
    /// The child entry `name` of this directory.
    pub fn join(&self, name: &str) -> Location {
        match self {
            Location::Local(path) => Location::Local(path.join(name)),
            Location::Remote(remote) => Location::Remote(remote.with_path(join_remote(&remote.path, name))),
        }
    }

    /// The parent directory, or `None` at the root (and at an unresolved `~`).
    pub fn parent(&self) -> Option<Location> {
        match self {
            Location::Local(path) => {
                path.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| Location::Local(p.to_path_buf()))
            }
            Location::Remote(remote) => {
                parent_remote(&remote.path).map(|path| Location::Remote(remote.with_path(path)))
            }
        }
    }

    /// "edge-proxy:/var/log" or "/home/me" — the syntax of `kraal cp`.
    pub fn display(&self) -> String {
        match self {
            Location::Local(path) => path.display().to_string(),
            Location::Remote(remote) => format!("{}:{}", remote.server, remote.path),
        }
    }

    /// Like [`Location::display`], with the home directory shown as `~`.
    fn short_display(&self) -> String {
        match self {
            Location::Local(path) => match home_dir().and_then(|home| path.strip_prefix(home).ok()) {
                Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
                Some(rest) => format!("~/{}", rest.display()),
                None => path.display().to_string(),
            },
            Location::Remote(_) => self.display(),
        }
    }

    /// The last path component: this directory's name in its parent.
    fn name(&self) -> Option<String> {
        match self {
            Location::Local(path) => path.file_name().map(|n| n.to_string_lossy().into_owned()),
            Location::Remote(remote) => {
                let trimmed = remote.path.trim_end_matches('/');
                if trimmed.is_empty() || (trimmed.starts_with('~') && !trimmed.contains('/')) {
                    return None;
                }
                trimmed.rsplit('/').next().map(str::to_owned)
            }
        }
    }
}

impl Remote {
    fn with_path(&self, path: String) -> Remote {
        Remote { server: self.server.clone(), ssh_args: self.ssh_args.clone(), path }
    }
}

/// Joins `/`-separated remote paths.
fn join_remote(base: &str, name: &str) -> String {
    if base.is_empty() {
        format!("~/{name}")
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// The parent of a `/`-separated remote path; relative paths are relative to `~`.
fn parent_remote(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || (trimmed.starts_with('~') && !trimmed.contains('/')) {
        // The root, or `~` / `~user` before the listing resolved it.
        return None;
    }
    match trimmed.rfind('/') {
        None => Some("~".to_owned()),
        Some(index) => {
            let parent = trimmed[..index].trim_end_matches('/');
            Some(if parent.is_empty() { "/".to_owned() } else { parent.to_owned() })
        }
    }
}

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// A directory, or a symbolic link to one.
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    /// unix seconds
    pub mtime: i64,
}

/// Keys the pane understands (the server maps its own key type to these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKey {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Backspace,
    Tab,
    Esc,
    Delete,
    Insert,
    Char(char),
    F(u8),
}

/// What the server must do after a key or click.
#[derive(Debug, Clone, PartialEq)]
pub enum FilesAction {
    None,
    /// Copy the marked entries (or the one under the cursor) of the active panel into the other panel's directory.
    Copy {
        sources: Vec<Location>,
        dest: Location,
    },
    /// Same, but move (the server asks for confirmation first).
    Move {
        sources: Vec<Location>,
        dest: Location,
    },
    /// Delete (the server asks for confirmation first).
    Delete(Vec<Location>),
    /// Open a local file in a preview pane (Enter / F3 on a local file).
    Preview(PathBuf),
    /// Open an SSH or local shell pane in this directory (key `s`).
    Shell(Location),
    /// Let the user pick a server for a panel (key `o` or `:`); the server answers with `set_location`.
    PickLocation(Side),
    Close,
}

/// One of the two panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    fn index(self) -> usize {
        match self {
            Side::Left => 0,
            Side::Right => 1,
        }
    }

    fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}

/// The result of a listing: the resolved directory and its entries, or an error message.
type Listing = Result<(Location, Vec<Entry>), String>;

/// A listing finished by a background thread.
struct Done {
    side: Side,
    request: u64,
    result: Listing,
}

/// How remote listings run.
#[derive(Debug, Clone)]
struct Ssh {
    program: String,
    timeout: Duration,
}

impl Default for Ssh {
    fn default() -> Self {
        Ssh { program: SSH_PROGRAM.to_owned(), timeout: SSH_TIMEOUT }
    }
}

/// A row of a panel listing.
#[derive(Clone, Copy)]
enum Row<'a> {
    Parent,
    Entry(&'a Entry),
}

/// One side of the pane: a directory listing with its cursor and marks.
struct Panel {
    location: Location,
    /// Every entry of the last listing, sorted.
    entries: Vec<Entry>,
    /// Indices into `entries` of the rows shown after the `..` row.
    shown: Vec<usize>,
    has_parent: bool,
    loading: bool,
    error: Option<String>,
    /// Names of the marked entries.
    marked: HashSet<String>,
    filter: String,
    /// Whether typed characters go to the filter.
    editing: bool,
    /// Row under the cursor; row 0 is `..` when `has_parent`.
    cursor: usize,
    /// First row shown, as of the last render.
    offset: usize,
    /// Id of the latest listing request; older results are dropped.
    request: u64,
    cancel: Arc<AtomicBool>,
    /// Entry to put the cursor on when the pending listing arrives.
    select: Option<String>,
}

impl Panel {
    fn new(location: Location) -> Panel {
        Panel {
            has_parent: location.parent().is_some(),
            location,
            entries: Vec::new(),
            shown: Vec::new(),
            loading: false,
            error: None,
            marked: HashSet::new(),
            filter: String::new(),
            editing: false,
            cursor: 0,
            offset: 0,
            request: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            select: None,
        }
    }

    /// Starts a new request, cancelling the previous one.
    fn supersede(&mut self) -> (u64, Arc<AtomicBool>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.request = self.request.wrapping_add(1);
        self.loading = true;
        (self.request, Arc::clone(&self.cancel))
    }

    /// Points the panel at `location`, forgetting the old listing.
    fn reset(&mut self, location: Location, select: Option<String>) {
        self.has_parent = location.parent().is_some();
        self.location = location;
        self.entries.clear();
        self.shown.clear();
        self.error = None;
        self.marked.clear();
        self.filter.clear();
        self.editing = false;
        self.cursor = 0;
        self.offset = 0;
        self.select = select;
    }

    fn apply(&mut self, result: Listing, show_hidden: bool) {
        self.loading = false;
        match result {
            Ok((location, mut entries)) => {
                sort_entries(&mut entries);
                let keep = self.select.take().or_else(|| self.current().map(|e| e.name.clone()));
                self.has_parent = location.parent().is_some();
                self.location = location;
                let names: HashSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
                self.marked.retain(|name| names.contains(name.as_str()));
                self.entries = entries;
                self.error = None;
                self.update_shown(show_hidden);
                self.restore_cursor(keep.as_deref());
            }
            Err(message) => {
                self.entries.clear();
                self.marked.clear();
                self.select = None;
                self.error = Some(message);
                self.update_shown(show_hidden);
                self.restore_cursor(None);
            }
        }
    }

    fn rows_len(&self) -> usize {
        usize::from(self.has_parent) + self.shown.len()
    }

    fn row(&self, index: usize) -> Option<Row<'_>> {
        if self.has_parent && index == 0 {
            return Some(Row::Parent);
        }
        let index = index.checked_sub(usize::from(self.has_parent))?;
        self.shown.get(index).and_then(|&i| self.entries.get(i)).map(Row::Entry)
    }

    /// The entry under the cursor (not `..`).
    fn current(&self) -> Option<&Entry> {
        match self.row(self.cursor)? {
            Row::Entry(entry) => Some(entry),
            Row::Parent => None,
        }
    }

    fn shown_entries(&self) -> impl Iterator<Item = &Entry> {
        self.shown.iter().filter_map(|&i| self.entries.get(i))
    }

    /// Marked entries that are currently shown, in display order.
    fn marked_entries(&self) -> Vec<&Entry> {
        self.shown_entries().filter(|e| self.marked.contains(&e.name)).collect()
    }

    fn row_of(&self, name: &str) -> Option<usize> {
        let index = self.shown.iter().position(|&i| self.entries.get(i).is_some_and(|e| e.name == name))?;
        Some(index + usize::from(self.has_parent))
    }

    /// The first row that is an entry (or `..` when there is none).
    fn first_entry_row(&self) -> usize {
        if self.shown.is_empty() { 0 } else { usize::from(self.has_parent) }
    }

    fn update_shown(&mut self, show_hidden: bool) {
        let filter = self.filter.to_lowercase();
        self.shown = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| show_hidden || !e.name.starts_with('.'))
            .filter(|(_, e)| filter.is_empty() || e.name.to_lowercase().contains(&filter))
            .map(|(i, _)| i)
            .collect();
    }

    /// Recomputes the shown rows, keeping the cursor on the same entry when possible.
    fn refilter(&mut self, show_hidden: bool) {
        let keep = self.current().map(|e| e.name.clone());
        self.update_shown(show_hidden);
        self.restore_cursor(keep.as_deref());
    }

    /// Changes the filter; the cursor stays on its entry if it still matches.
    fn set_filter(&mut self, filter: String, show_hidden: bool) {
        let keep = self.current().map(|e| e.name.clone());
        self.filter = filter;
        self.update_shown(show_hidden);
        self.cursor = keep.and_then(|name| self.row_of(&name)).unwrap_or_else(|| self.first_entry_row());
    }

    fn restore_cursor(&mut self, name: Option<&str>) {
        if let Some(row) = name.and_then(|name| self.row_of(name)) {
            self.cursor = row;
        } else {
            self.cursor = self.cursor.min(self.rows_len().saturating_sub(1));
        }
    }

    fn move_by(&mut self, delta: isize) {
        let target = self.cursor.saturating_add_signed(delta);
        self.cursor = target.min(self.rows_len().saturating_sub(1));
    }

    /// The first row to show in a list of `height` rows so that the cursor is visible.
    fn scroll_offset(&self, height: usize) -> usize {
        let rows = self.rows_len();
        let mut offset = self.offset;
        if height == 0 {
            return offset.min(rows.saturating_sub(1));
        }
        if self.cursor < offset {
            offset = self.cursor;
        } else if self.cursor >= offset.saturating_add(height) {
            offset = self.cursor + 1 - height;
        }
        offset.min(rows.saturating_sub(height))
    }

    /// Lines shown after the rows: loading, error or empty notes.
    fn messages(&self, width: usize) -> Vec<(String, Style)> {
        if let Some(error) = &self.error {
            return wrap(&sanitize_lines(error), width).into_iter().map(|line| (line, ERROR)).collect();
        }
        if !self.shown.is_empty() {
            return Vec::new();
        }
        let note = if self.loading {
            "loading…"
        } else if self.entries.is_empty() {
            "(empty)"
        } else if !self.filter.is_empty() {
            "(no match)"
        } else {
            "(only hidden files, press .)"
        };
        vec![(note.to_owned(), NOTE)]
    }

    /// The panel's status line.
    fn status(&self, show_hidden: bool) -> (String, Style) {
        if self.editing {
            return (format!("/{}▏", sanitize(&self.filter)), FILTER);
        }
        if self.error.is_some() || (self.loading && self.entries.is_empty()) {
            return (String::new(), DIM);
        }
        let mut parts = Vec::new();
        let marked = self.marked_entries();
        let style = if marked.is_empty() {
            let files = self.shown_entries().filter(|e| !e.is_dir);
            let size: u64 = files.map(|e| e.size).fold(0, u64::saturating_add);
            let count = self.shown.len();
            parts.push(format!("{count} item{}, {}", if count == 1 { "" } else { "s" }, human_size(size)));
            DIM
        } else {
            // Directory sizes are unknown: the size counts the marked files only.
            let files: Vec<&&Entry> = marked.iter().filter(|e| !e.is_dir).collect();
            let size: u64 = files.iter().map(|e| e.size).fold(0, u64::saturating_add);
            if files.is_empty() {
                parts.push(format!("{} marked", marked.len()));
            } else {
                parts.push(format!("{} marked, {}", marked.len(), human_size(size)));
            }
            MARKED
        };
        if !self.filter.is_empty() {
            parts.push(format!("filter: {}", sanitize(&self.filter)));
        }
        let hidden = self.entries.iter().filter(|e| e.name.starts_with('.')).count();
        if !show_hidden && hidden > 0 {
            parts.push(format!("{hidden} hidden"));
        }
        (parts.join(" · "), style)
    }
}

/// Directories first, then by name ignoring case.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase(), e.name.clone()));
}

/// A two-panel file manager.
pub struct FilesPane {
    panels: [Panel; 2],
    active: Side,
    show_hidden: bool,
    /// Size of the last rendered area, if any.
    view: Option<(u16, u16)>,
    /// Listings finished by background threads, applied by `poll`.
    done: Arc<Mutex<Vec<Done>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    ssh: Ssh,
}

impl fmt::Debug for FilesPane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilesPane")
            .field("left", &self.panels[0].location)
            .field("right", &self.panels[1].location)
            .field("active", &self.active)
            .field("view", &self.view)
            .finish_non_exhaustive()
    }
}

impl Drop for FilesPane {
    fn drop(&mut self) {
        // Kills listings still running.
        for panel in &self.panels {
            panel.cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl FilesPane {
    /// `wake` is called from background threads when a listing finished, so the server redraws.
    pub fn new(left: Location, right: Location, wake: Arc<dyn Fn() + Send + Sync>) -> FilesPane {
        FilesPane::with_ssh(left, right, wake, Ssh::default())
    }

    fn with_ssh(left: Location, right: Location, wake: Arc<dyn Fn() + Send + Sync>, ssh: Ssh) -> FilesPane {
        let mut pane = FilesPane {
            panels: [Panel::new(left), Panel::new(right)],
            active: Side::Left,
            show_hidden: false,
            view: None,
            done: Arc::new(Mutex::new(Vec::new())),
            wake,
            ssh,
        };
        pane.load(Side::Left);
        pane.load(Side::Right);
        pane
    }

    /// E.g. "files · ~/Dev ⇄ edge-proxy:/var/log".
    pub fn title(&self) -> String {
        let [left, right] = &self.panels;
        format!("files · {} ⇄ {}", left.location.short_display(), right.location.short_display())
    }

    #[cfg(test)]
    pub fn active(&self) -> Side {
        self.active
    }

    pub fn location(&self, side: Side) -> &Location {
        &self.panel(side).location
    }

    /// Points a panel at `loc` and lists it.
    pub fn set_location(&mut self, side: Side, loc: Location) {
        self.navigate(side, loc, None);
    }

    /// Reloads both panels, keeping cursors and marks where the names still exist.
    pub fn refresh(&mut self) {
        self.load(Side::Left);
        self.load(Side::Right);
    }

    /// Applies finished background listings; true if something changed.
    pub fn poll(&mut self) -> bool {
        let done = mem::take(&mut *self.done.lock().unwrap_or_else(PoisonError::into_inner));
        let mut changed = false;
        for item in done {
            if self.panel(item.side).request == item.request {
                let show_hidden = self.show_hidden;
                self.panel_mut(item.side).apply(item.result, show_hidden);
                changed = true;
            }
        }
        changed
    }

    /// Fills `area` of `buf` with both panels and the key hints. Never writes outside `area`.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        self.view = Some((area.width, area.height));
        let height = usize::from(geometry(area, self.active).list.1);
        for panel in &mut self.panels {
            panel.offset = panel.scroll_offset(height);
        }
        self.draw(area, buf);
    }

    /// Text of what is shown, for tests and the CLI `read` command.
    ///
    /// Uses the last rendered size (80×24 before the first render).
    pub fn visible_text(&self) -> String {
        let (width, height) = self.view.unwrap_or(DEFAULT_VIEW);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        self.draw(area, &mut buf);
        buffer_rows(&buf, area).join("\n")
    }

    /// Handles a key; the returned action is for the server to carry out.
    pub fn key(&mut self, key: FileKey) -> FilesAction {
        let side = self.active;
        let show_hidden = self.show_hidden;
        if self.panel(side).editing {
            let panel = self.panel_mut(side);
            match key {
                FileKey::Char(c) => {
                    let mut filter = panel.filter.clone();
                    filter.push(c);
                    panel.set_filter(filter, show_hidden);
                    return FilesAction::None;
                }
                FileKey::Backspace => {
                    let mut filter = panel.filter.clone();
                    if filter.pop().is_none() {
                        panel.editing = false;
                    }
                    panel.set_filter(filter, show_hidden);
                    return FilesAction::None;
                }
                FileKey::Esc => {
                    panel.editing = false;
                    panel.set_filter(String::new(), show_hidden);
                    return FilesAction::None;
                }
                FileKey::Enter => {
                    panel.editing = false;
                    return FilesAction::None;
                }
                // Navigation keys keep working while typing.
                _ => {}
            }
        }
        let page = isize::try_from(self.list_height().saturating_sub(1).max(1)).unwrap_or(1);
        match key {
            FileKey::Up | FileKey::Char('k') => self.panel_mut(side).move_by(-1),
            FileKey::Down | FileKey::Char('j') => self.panel_mut(side).move_by(1),
            FileKey::PageUp => self.panel_mut(side).move_by(-page),
            FileKey::PageDown => self.panel_mut(side).move_by(page),
            FileKey::Home => self.panel_mut(side).cursor = 0,
            FileKey::End => {
                let panel = self.panel_mut(side);
                panel.cursor = panel.rows_len().saturating_sub(1);
            }
            FileKey::Enter | FileKey::F(3) => return self.open(side),
            FileKey::Backspace => self.go_up(side),
            FileKey::Tab => {
                self.panel_mut(side).editing = false;
                self.active = side.other();
            }
            FileKey::Char(' ') | FileKey::Insert => {
                let panel = self.panel_mut(side);
                if let Some(name) = panel.current().map(|e| e.name.clone())
                    && !panel.marked.remove(&name)
                {
                    panel.marked.insert(name);
                }
                panel.move_by(1);
            }
            FileKey::Char('*') => {
                let panel = self.panel_mut(side);
                let names: Vec<String> = panel.shown_entries().map(|e| e.name.clone()).collect();
                if names.iter().all(|name| panel.marked.contains(name)) {
                    for name in &names {
                        panel.marked.remove(name);
                    }
                } else {
                    panel.marked.extend(names);
                }
            }
            FileKey::Char('c') | FileKey::F(5) => return self.transfer(side, false),
            FileKey::Char('m') | FileKey::F(6) => return self.transfer(side, true),
            FileKey::Char('d') | FileKey::Delete | FileKey::F(8) => {
                let sources = self.sources(side);
                if !sources.is_empty() {
                    return FilesAction::Delete(sources);
                }
            }
            FileKey::Char('s') => return FilesAction::Shell(self.panel(side).location.clone()),
            FileKey::Char('o' | ':') => return FilesAction::PickLocation(side),
            FileKey::Char('r') => self.refresh(),
            FileKey::Char('.') => {
                self.show_hidden = !self.show_hidden;
                for panel in &mut self.panels {
                    panel.refilter(self.show_hidden);
                }
            }
            FileKey::Char('/') => {
                let panel = self.panel_mut(side);
                panel.editing = true;
            }
            FileKey::Esc if !self.panel(side).filter.is_empty() => {
                self.panel_mut(side).set_filter(String::new(), show_hidden);
            }
            FileKey::Char('q') | FileKey::Esc | FileKey::F(10) => return FilesAction::Close,
            _ => {}
        }
        FilesAction::None
    }

    /// Mouse: click selects the panel and the entry (double click = Enter); wheel scrolls.
    ///
    /// `col` and `row` are relative to the area last passed to [`FilesPane::render`].
    /// A click on a key hint presses that key.
    pub fn click(&mut self, col: u16, row: u16, double: bool) -> FilesAction {
        let geo = self.geometry();
        if geo.footer == Some(row) {
            let hints = self.hints();
            let hit =
                footer_layout(&hints, geo.area.width).into_iter().find(|&(start, end, _)| (start..end).contains(&col));
            return match hit.and_then(|(_, _, index)| hints.get(index)).and_then(|hint| hint.key) {
                Some(key) => self.key(key),
                None => FilesAction::None,
            };
        }
        let Some(side) = geo.side_at(col) else {
            return FilesAction::None;
        };
        if side != self.active {
            self.panel_mut(self.active).editing = false;
            self.active = side;
        }
        let (list_y, list_height) = geo.list;
        if row < list_y || row - list_y >= list_height {
            return FilesAction::None;
        }
        let panel = self.panel_mut(side);
        let index = panel.scroll_offset(usize::from(list_height)).saturating_add(usize::from(row - list_y));
        if index >= panel.rows_len() {
            return FilesAction::None;
        }
        panel.cursor = index;
        if double { self.open(side) } else { FilesAction::None }
    }

    /// Scrolls the panel under column `col` by `delta` rows (negative is up).
    /// The cursor is kept within the visible rows.
    pub fn scroll(&mut self, col: u16, delta: i32) {
        let geo = self.geometry();
        let side = geo.side_at(col).unwrap_or(self.active);
        let height = usize::from(geo.list.1).max(1);
        let panel = self.panel_mut(side);
        let rows = panel.rows_len();
        let max_offset = rows.saturating_sub(height);
        let delta = isize::try_from(delta).unwrap_or(0);
        panel.offset = panel.scroll_offset(height).saturating_add_signed(delta).min(max_offset);
        let last_visible = (panel.offset + height - 1).min(rows.saturating_sub(1));
        panel.cursor = panel.cursor.clamp(panel.offset.min(last_visible), last_visible);
    }

    fn panel(&self, side: Side) -> &Panel {
        &self.panels[side.index()]
    }

    fn panel_mut(&mut self, side: Side) -> &mut Panel {
        &mut self.panels[side.index()]
    }

    /// Layout of the last rendered size, relative to the rendered area.
    fn geometry(&self) -> Geometry {
        let (width, height) = self.view.unwrap_or(DEFAULT_VIEW);
        geometry(Rect::new(0, 0, width, height), self.active)
    }

    fn list_height(&self) -> usize {
        usize::from(self.geometry().list.1)
    }

    fn navigate(&mut self, side: Side, location: Location, select: Option<String>) {
        self.panel_mut(side).reset(location, select);
        self.load(side);
    }

    /// Lists the panel's location on a background thread.
    fn load(&mut self, side: Side) {
        let panel = self.panel_mut(side);
        let (request, cancel) = panel.supersede();
        let location = panel.location.clone();
        let (done, wake, ssh) = (Arc::clone(&self.done), Arc::clone(&self.wake), self.ssh.clone());
        let spawned = thread::Builder::new().name("kraal-files".to_owned()).spawn(move || {
            let result = list(&location, &ssh, &cancel);
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            done.lock().unwrap_or_else(PoisonError::into_inner).push(Done { side, request, result });
            wake();
        });
        if let Err(err) = spawned {
            let show_hidden = self.show_hidden;
            self.panel_mut(side).apply(Err(format!("cannot start listing: {err}")), show_hidden);
        }
    }

    /// Enter: opens the directory under the cursor, or previews a local file.
    fn open(&mut self, side: Side) -> FilesAction {
        let panel = self.panel(side);
        match panel.row(panel.cursor) {
            Some(Row::Parent) => self.go_up(side),
            Some(Row::Entry(entry)) if entry.is_dir => {
                let target = panel.location.join(&entry.name);
                self.navigate(side, target, None);
            }
            Some(Row::Entry(entry)) => {
                if let Location::Local(path) = panel.location.join(&entry.name) {
                    return FilesAction::Preview(path);
                }
            }
            None => {}
        }
        FilesAction::None
    }

    /// Goes to the parent directory, with the cursor on the directory left.
    fn go_up(&mut self, side: Side) {
        let location = &self.panel(side).location;
        if let Some(parent) = location.parent() {
            let name = location.name();
            self.navigate(side, parent, name);
        }
    }

    /// The marked entries, or else the one under the cursor; never `..`.
    fn sources(&self, side: Side) -> Vec<Location> {
        let panel = self.panel(side);
        let marked = panel.marked_entries();
        let entries = if marked.is_empty() { panel.current().into_iter().collect() } else { marked };
        entries.into_iter().map(|e| panel.location.join(&e.name)).collect()
    }

    fn transfer(&mut self, side: Side, is_move: bool) -> FilesAction {
        let sources = self.sources(side);
        let dest = self.panel(side.other()).location.clone();
        if sources.is_empty() || dest == self.panel(side).location {
            return FilesAction::None;
        }
        if is_move {
            FilesAction::Move { sources, dest }
        } else {
            // Marks are dropped once a copy is under way; moved or deleted
            // entries lose theirs when the listing is refreshed.
            self.panel_mut(side).marked.clear();
            FilesAction::Copy { sources, dest }
        }
    }

    // -----------------------------------------------------------------------
    // Drawing

    fn draw(&self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].reset();
            }
        }
        if area.is_empty() {
            return;
        }
        let geo = geometry(area, self.active);
        for side in [Side::Left, Side::Right] {
            self.draw_panel(side, geo.panels[side.index()], &geo, buf);
        }
        if let Some(x) = geo.separator {
            let bottom = geo.footer.unwrap_or(area.bottom());
            for y in area.top()..bottom {
                put(buf, area, x, y, "│", 1, DIM);
            }
        }
        if let Some(y) = geo.footer {
            let hints = self.hints();
            let width = usize::from(area.width);
            for (n, (start, end, index)) in footer_layout(&hints, area.width).into_iter().enumerate() {
                let Some(hint) = hints.get(index) else { continue };
                if n > 0 {
                    put(buf, area, area.x.saturating_add(start.saturating_sub(3)), y, " · ", width, HINT_TEXT);
                }
                let after_key = put(buf, area, area.x.saturating_add(start), y, hint.label, width, HINT_KEY);
                let text_width = usize::from(end.saturating_sub(start)).saturating_sub(text_width(hint.label));
                put(buf, area, after_key, y, &format!(" {}", hint.text), text_width, HINT_TEXT);
            }
        }
    }

    fn draw_panel(&self, side: Side, rect: Rect, geo: &Geometry, buf: &mut Buffer) {
        if rect.is_empty() {
            return;
        }
        let panel = self.panel(side);
        let active = side == self.active;
        let width = usize::from(rect.width);
        let inner = width.saturating_sub(1);
        if let Some(y) = geo.header {
            let style = if active { HEADER_ACTIVE } else { HEADER_INACTIVE };
            buf.set_style(Rect::new(rect.x, y, rect.width, 1).intersection(rect), style);
            let mut text = sanitize(&panel.location.short_display());
            if panel.loading && !panel.entries.is_empty() {
                text.push_str("  loading…");
            }
            put(buf, rect, rect.x.saturating_add(1), y, &truncate_start(&text, width.saturating_sub(2)), inner, style);
        }
        let columns = Columns::new(width);
        if let Some(y) = geo.columns {
            put(buf, rect, rect.x.saturating_add(1), y, &truncate("Name", columns.name), columns.name, DIM);
            if let Some(x) = columns.size_x() {
                put(buf, rect, rect.x.saturating_add(x), y, &pad_start("Size", SIZE_WIDTH), SIZE_WIDTH, DIM);
            }
            if let Some(x) = columns.date_x() {
                put(buf, rect, rect.x.saturating_add(x), y, "Modified (UTC)", DATE_WIDTH, DIM);
            }
        }
        let (list_y, list_height) = geo.list;
        let offset = panel.scroll_offset(usize::from(list_height));
        let messages = panel.messages(width.saturating_sub(2).max(1));
        let rows = panel.rows_len();
        for (index, y) in (offset..).zip(list_y..list_y.saturating_add(list_height)) {
            match panel.row(index) {
                Some(row) => {
                    let cursor = index == panel.cursor;
                    self.draw_row(panel, row, RowPlace { rect, y, cursor, active, columns }, buf);
                }
                None => {
                    if let Some((text, style)) = index.checked_sub(rows).and_then(|i| messages.get(i)) {
                        put(buf, rect, rect.x.saturating_add(1), y, text, inner, *style);
                    }
                }
            }
        }
        if let Some(y) = geo.status {
            let (text, style) = panel.status(self.show_hidden);
            put(buf, rect, rect.x.saturating_add(1), y, &truncate(&text, inner), inner, style);
        }
    }

    fn draw_row(&self, panel: &Panel, row: Row<'_>, place: RowPlace, buf: &mut Buffer) {
        let RowPlace { rect, y, cursor, active, columns } = place;
        let (name, size, date, mut style) = match row {
            Row::Parent => ("..".to_owned(), "UP--DIR".to_owned(), String::new(), DIR_NAME),
            Row::Entry(entry) => {
                let mut name = sanitize(&entry.name);
                if entry.is_dir {
                    name.push('/');
                } else if entry.is_link {
                    name.push('@');
                }
                let size = if entry.is_dir { "<DIR>".to_owned() } else { human_size(entry.size) };
                let mut style = if panel.marked.contains(&entry.name) {
                    MARKED
                } else if entry.is_link {
                    LINK_NAME
                } else if entry.is_dir {
                    DIR_NAME
                } else {
                    Style::new()
                };
                if entry.name.starts_with('.') {
                    style = style.add_modifier(Modifier::DIM);
                }
                (name, size, format_time(entry.mtime), style)
            }
        };
        let marked = matches!(row, Row::Entry(e) if panel.marked.contains(&e.name));
        let mut detail = if marked { MARKED } else { Style::new() };
        if cursor {
            let highlight = if active { CURSOR_ACTIVE } else { CURSOR_INACTIVE };
            buf.set_style(Rect::new(rect.x, y, rect.width, 1).intersection(rect), highlight);
            style = style.patch(highlight);
            detail = detail.patch(highlight);
            if active && marked {
                style = style.fg(Color::Yellow);
                detail = detail.fg(Color::Yellow);
            }
        }
        put(buf, rect, rect.x.saturating_add(1), y, &truncate(&name, columns.name), columns.name, style);
        if let Some(x) = columns.size_x() {
            put(buf, rect, rect.x.saturating_add(x), y, &pad_start(&size, SIZE_WIDTH), SIZE_WIDTH, detail);
        }
        if let Some(x) = columns.date_x() {
            put(buf, rect, rect.x.saturating_add(x), y, &truncate(&date, DATE_WIDTH), DATE_WIDTH, detail);
        }
    }

    fn hints(&self) -> Vec<Hint> {
        if self.panel(self.active).editing {
            return vec![
                Hint::new("type", "to filter", None),
                Hint::new("enter", "keep", Some(FileKey::Enter)),
                Hint::new("esc", "clear", Some(FileKey::Esc)),
            ];
        }
        let arrow = if self.active == Side::Left { "→" } else { "←" };
        vec![
            Hint::new("tab", "switch", Some(FileKey::Tab)),
            Hint::new("enter", "open", Some(FileKey::Enter)),
            Hint::new("⌫", "up", Some(FileKey::Backspace)),
            Hint::new("space", "mark", Some(FileKey::Char(' '))),
            Hint { label: "c", text: format!("copy {arrow}"), key: Some(FileKey::Char('c')) },
            Hint::new("m", "move", Some(FileKey::Char('m'))),
            Hint::new("d", "delete", Some(FileKey::Char('d'))),
            Hint::new("s", "shell", Some(FileKey::Char('s'))),
            Hint::new("o", "server", Some(FileKey::Char('o'))),
            Hint::new("r", "refresh", Some(FileKey::Char('r'))),
            Hint::new(".", "hidden", Some(FileKey::Char('.'))),
            Hint::new("/", "filter", Some(FileKey::Char('/'))),
            Hint::new("q", "close", Some(FileKey::Char('q'))),
        ]
    }
}

/// Where a list row is drawn.
#[derive(Clone, Copy)]
struct RowPlace {
    rect: Rect,
    y: u16,
    cursor: bool,
    active: bool,
    columns: Columns,
}

/// A key hint of the footer.
struct Hint {
    label: &'static str,
    text: String,
    key: Option<FileKey>,
}

impl Hint {
    fn new(label: &'static str, text: &str, key: Option<FileKey>) -> Hint {
        Hint { label, text: text.to_owned(), key }
    }

    fn width(&self) -> u16 {
        u16::try_from(text_width(self.label) + 1 + text_width(&self.text)).unwrap_or(u16::MAX)
    }
}

/// Column ranges (relative to the area) of the hints that fit in `width`, with their index.
fn footer_layout(hints: &[Hint], width: u16) -> Vec<(u16, u16, usize)> {
    let mut placed = Vec::new();
    let mut x: u16 = 1;
    for (index, hint) in hints.iter().enumerate() {
        let end = x.saturating_add(hint.width());
        if end > width {
            break;
        }
        placed.push((x, end, index));
        x = end.saturating_add(3);
    }
    placed
}

/// Widths of a panel's columns: ` name size date`.
#[derive(Clone, Copy)]
struct Columns {
    name: usize,
    size: bool,
    date: bool,
}

impl Columns {
    fn new(width: usize) -> Columns {
        let inner = width.saturating_sub(1);
        let with_date = 1 + SIZE_WIDTH + 1 + DATE_WIDTH + 1;
        let with_size = 1 + SIZE_WIDTH + 1;
        if inner >= MIN_NAME_WIDTH + with_date {
            Columns { name: inner - with_date, size: true, date: true }
        } else if inner >= MIN_NAME_WIDTH + with_size {
            Columns { name: inner - with_size, size: true, date: false }
        } else {
            Columns { name: inner, size: false, date: false }
        }
    }

    /// Column of the size, relative to the panel.
    fn size_x(&self) -> Option<u16> {
        self.size.then(|| u16::try_from(1 + self.name + 1).unwrap_or(u16::MAX))
    }

    fn date_x(&self) -> Option<u16> {
        self.date.then(|| u16::try_from(1 + self.name + 1 + SIZE_WIDTH + 1).unwrap_or(u16::MAX))
    }
}

/// Rows and panel rectangles of the pane for one area.
struct Geometry {
    area: Rect,
    panels: [Rect; 2],
    separator: Option<u16>,
    header: Option<u16>,
    columns: Option<u16>,
    /// First row and height of the listings.
    list: (u16, u16),
    status: Option<u16>,
    footer: Option<u16>,
}

impl Geometry {
    fn side_at(&self, col: u16) -> Option<Side> {
        let inside = |rect: Rect| col >= rect.left() && col < rect.right();
        if inside(self.panels[0]) {
            Some(Side::Left)
        } else if inside(self.panels[1]) {
            Some(Side::Right)
        } else {
            None
        }
    }
}

/// Lays out `area`: rows are dropped from the least useful as it shrinks, and
/// narrow areas show only the `active` panel.
fn geometry(area: Rect, active: Side) -> Geometry {
    let height = area.height;
    let mut top = area.y;
    let mut bottom = area.bottom();
    let header = (height >= 1).then(|| {
        top += 1;
        area.y
    });
    let footer = (height >= 3).then(|| {
        bottom -= 1;
        bottom
    });
    let status = (height >= 5).then(|| {
        bottom -= 1;
        bottom
    });
    let columns = (height >= 7).then(|| {
        top += 1;
        top - 1
    });
    let list = (top, bottom.saturating_sub(top));
    let panel_height = footer.map_or(height, |y| y - area.y);
    let (panels, separator) = if area.width >= MIN_SPLIT_WIDTH {
        let left = (area.width - 1) / 2;
        let right = area.width - 1 - left;
        let separator = area.x + left;
        (
            [Rect::new(area.x, area.y, left, panel_height), Rect::new(separator + 1, area.y, right, panel_height)],
            Some(separator),
        )
    } else {
        let full = Rect::new(area.x, area.y, area.width, panel_height);
        let none = Rect::new(area.x, area.y, 0, 0);
        (if active == Side::Left { [full, none] } else { [none, full] }, None)
    };
    Geometry { area, panels, separator, header, columns, list, status, footer }
}

/// Writes `text` at (`x`, `y`) within `clip`, at most `max_width` columns;
/// returns the column after the text.
fn put(buf: &mut Buffer, clip: Rect, x: u16, y: u16, text: &str, max_width: usize, style: Style) -> u16 {
    let clip = clip.intersection(buf.area);
    if y < clip.top() || y >= clip.bottom() || x < clip.left() || x >= clip.right() {
        return x;
    }
    let max_width = max_width.min(usize::from(clip.right() - x));
    buf.set_stringn(x, y, text, max_width, style).0
}

/// Rows of `area` as text, skipping the cells hidden by wide characters.
fn buffer_rows(buf: &Buffer, area: Rect) -> Vec<String> {
    let area = area.intersection(buf.area);
    (area.top()..area.bottom())
        .map(|y| {
            let mut row = String::new();
            let mut x = area.left();
            while x < area.right() {
                let symbol = buf[(x, y)].symbol();
                row.push_str(symbol);
                let width = symbol.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
                x = x.saturating_add(u16::try_from(width.max(1)).unwrap_or(1));
            }
            row.trim_end().to_owned()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Listing

/// Lists `location`; runs on a background thread.
fn list(location: &Location, ssh: &Ssh, cancel: &AtomicBool) -> Listing {
    match location {
        Location::Local(path) => {
            let (path, entries) = list_local(path)?;
            Ok((Location::Local(path), entries))
        }
        Location::Remote(remote) => {
            let (path, entries) = list_remote(remote, ssh, cancel)?;
            Ok((Location::Remote(remote.with_path(path)), entries))
        }
    }
}

fn list_local(path: &Path) -> Result<(PathBuf, Vec<Entry>), String> {
    let path = resolve_local(path);
    let reader = fs::read_dir(&path).map_err(|err| format!("cannot list {}: {err}", path.display()))?;
    let entries = reader
        .filter_map(Result::ok)
        .map(|entry| {
            let entry_path = entry.path();
            // Size and date are the link's own, like `ls -l` and the remote listing;
            // links are followed to tell whether they can be entered.
            let own = fs::symlink_metadata(&entry_path).ok();
            let is_link = own.as_ref().is_some_and(|m| m.file_type().is_symlink());
            let is_dir = if is_link {
                fs::metadata(&entry_path).is_ok_and(|m| m.is_dir())
            } else {
                own.as_ref().is_some_and(fs::Metadata::is_dir)
            };
            Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir,
                is_link,
                size: own.as_ref().map_or(0, fs::Metadata::len),
                mtime: own.and_then(|m| m.modified().ok()).map_or(0, unix_seconds),
            }
        })
        .collect();
    Ok((path, entries))
}

/// Expands `~`, makes the path absolute and removes `.` and `..` components.
fn resolve_local(path: &Path) -> PathBuf {
    let expanded = match (path.strip_prefix("~"), home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    };
    let absolute = std::path::absolute(&expanded).unwrap_or(expanded);
    let mut normal = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other.as_os_str()),
        }
    }
    normal
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|home| !home.is_empty()).map(PathBuf::from)
}

fn list_remote(remote: &Remote, ssh: &Ssh, cancel: &AtomicBool) -> Result<(String, Vec<Entry>), String> {
    let args = ssh_command_args(remote).ok_or_else(|| format!("no ssh destination for {}", remote.server))?;
    let output = run_with_timeout(&ssh.program, &args, ssh.timeout, cancel)?;
    match parse_listing(&output.stdout, unix_seconds(SystemTime::now())) {
        Some((path, entries)) if !entries.is_empty() || output.status.success() => Ok((path, entries)),
        _ => Err(failure_message(&output)),
    }
}

/// Arguments for ssh: the user's options, ours, the destination, then the remote command.
fn ssh_command_args(remote: &Remote) -> Option<Vec<String>> {
    let (destination, options) = remote.ssh_args.split_last()?;
    let mut args = options.to_vec();
    args.extend(SSH_OPTIONS.iter().map(|option| (*option).to_owned()));
    args.push(destination.clone());
    args.push(remote_command(&remote.path));
    Some(args)
}

/// The command ssh runs: a POSIX script, wrapped in `sh -c` so that it works
/// whatever the login shell (fish, csh, …).
fn remote_command(path: &str) -> String {
    format!("sh -c {}", shell_quote(&remote_script(path)))
}

/// Prints `MARKER`, the resolved directory, then the entries: with GNU find,
/// `F` and `type\tsize\tmtime\tname` records; otherwise `L`, the output of
/// `ls -lan` and the names of the links to directories. Fields are separated
/// by NUL, which cannot appear in names.
fn remote_script(path: &str) -> String {
    let cd = format!("cd -- {} || exit 1", remote_dir_arg(path));
    [
        cd.as_str(),
        "printf 'KRAAL:LIST\\0%s\\0' \"$PWD\"",
        "if find . -maxdepth 0 -printf '' >/dev/null 2>&1",
        "then printf 'F\\0'",
        "find . -mindepth 1 -maxdepth 1 -printf '%y%Y\\t%s\\t%T@\\t%f\\0' || exit 1",
        "else printf 'L\\0'",
        "LC_ALL=C ls -lan || exit 1",
        "printf '\\0'",
        "for f in .* *; do case $f in .|..) continue;; esac; \
         [ -L \"$f\" ] && [ -d \"$f\" ] && printf '%s\\0' \"$f\"; done",
        "fi",
        "exit 0",
    ]
    .join("; ")
}

/// A shell word for the remote directory: `~` stays expandable, the rest is quoted.
fn remote_dir_arg(path: &str) -> String {
    if path.is_empty() || path == "~" {
        return "\"$HOME\"".to_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", shell_quote(rest));
    }
    if let Some(rest) = path.strip_prefix('~') {
        let (user, tail) = rest.split_once('/').unwrap_or((rest, ""));
        if !user.is_empty() && user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return format!("~{user}/{}", shell_quote(tail));
        }
    }
    shell_quote(path)
}

/// Single-quotes `text` for a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: ExitStatus,
}

/// Runs `program`, killing it after `timeout` or when `cancel` is set.
fn run_with_timeout(program: &str, args: &[String], timeout: Duration, cancel: &AtomicBool) -> Result<Output, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("cannot run {program}: {err}"))?;
    let kill = |child: &mut std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel::<()>();
    // Read stderr incrementally: a backgrounded ControlPersist master may keep it open.
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let readers = (child.stdout.take(), child.stderr.take());
    let (Some(mut stdout_pipe), Some(mut stderr_pipe)) = readers else {
        kill(&mut child);
        return Err(format!("cannot read the output of {program}"));
    };
    let sink = Arc::clone(&stderr);
    let spawned = thread::Builder::new()
        .spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut bytes);
            let _ = stdout_tx.send(bytes);
        })
        .and_then(|_| {
            thread::Builder::new().spawn(move || {
                let mut chunk = [0; 4096];
                loop {
                    match stderr_pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => {
                            let mut sink = sink.lock().unwrap_or_else(PoisonError::into_inner);
                            let room = MAX_STDERR.saturating_sub(sink.len()).min(read);
                            sink.extend_from_slice(&chunk[..room]);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let _ = stderr_tx.send(());
            })
        });
    if let Err(err) = spawned {
        kill(&mut child);
        return Err(format!("cannot read the output of {program}: {err}"));
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => {
                kill(&mut child);
                return Err(format!("cannot wait for {program}: {err}"));
            }
        }
        if cancel.load(Ordering::Relaxed) {
            kill(&mut child);
            return Err("cancelled".to_owned());
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            return Err(format!("timed out after {timeout:?}"));
        }
        thread::sleep(POLL_INTERVAL);
    };
    let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_secs(1));
    let stdout = stdout_rx.recv_timeout(left).map_err(|_| format!("{program} kept its output open"))?;
    let _ = stderr_rx.recv_timeout(Duration::from_millis(200));
    let stderr = stderr.lock().unwrap_or_else(PoisonError::into_inner).clone();
    Ok(Output { stdout, stderr, status })
}

/// The last lines of stderr, or the exit status when there are none.
fn failure_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    if lines.is_empty() {
        return match output.status.code() {
            Some(255) => "ssh failed (exit status 255)".to_owned(),
            Some(code) => format!("listing failed (exit status {code})"),
            None => "listing failed (killed by a signal)".to_owned(),
        };
    }
    lines[lines.len().saturating_sub(3)..].join("\n")
}

/// Parses the output of [`remote_script`] into the resolved directory and its entries.
fn parse_listing(stdout: &[u8], now: i64) -> Option<(String, Vec<Entry>)> {
    let start = stdout.windows(MARKER.len()).position(|window| window == MARKER)? + MARKER.len();
    let mut fields = stdout[start..].splitn(3, |&b| b == 0);
    let dir = String::from_utf8_lossy(fields.next()?).into_owned();
    let mode = fields.next()?;
    let body = fields.next().unwrap_or_default();
    if dir.is_empty() {
        return None;
    }
    let entries = match mode {
        b"F" => body.split(|&b| b == 0).filter_map(parse_find_record).collect(),
        b"L" => parse_ls(body, now),
        _ => return None,
    };
    Some((dir, entries))
}

/// Parses one `%y%Y\t%s\t%T@\t%f` record of GNU find.
fn parse_find_record(record: &[u8]) -> Option<Entry> {
    let mut fields = record.splitn(4, |&b| b == b'\t');
    let (kind, size, mtime, name) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?);
    let name = String::from_utf8_lossy(name).into_owned();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let own = *kind.first()?;
    let target = kind.get(1).copied().unwrap_or(own);
    let mtime = std::str::from_utf8(mtime).ok()?.split('.').next()?.parse().unwrap_or(0);
    Some(Entry {
        name,
        is_dir: target == b'd',
        is_link: own == b'l',
        size: std::str::from_utf8(size).ok().and_then(|s| s.parse().ok()).unwrap_or(0),
        mtime,
    })
}

/// Parses the `ls -lan` fallback: the listing, then NUL-separated names of links to directories.
fn parse_ls(body: &[u8], now: i64) -> Vec<Entry> {
    let mut parts = body.splitn(2, |&b| b == 0);
    let text = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
    let link_dirs: HashSet<String> = parts
        .next()
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    text.lines()
        .filter_map(|line| parse_ls_line(line, now))
        .map(|mut entry| {
            if entry.is_link && link_dirs.contains(&entry.name) {
                entry.is_dir = true;
            }
            entry
        })
        .collect()
}

/// Parses a line of `ls -lan`: `perms links uid gid size month day time|year name`.
fn parse_ls_line(line: &str, now: i64) -> Option<Entry> {
    let mut rest = line;
    let mut next = || {
        let start = rest.len() - rest.trim_start().len();
        let token_len = rest[start..].find(char::is_whitespace).unwrap_or(rest.len() - start);
        let token = &rest[start..start + token_len];
        rest = &rest[start + token_len..];
        (!token.is_empty()).then_some(token)
    };
    let perms = next()?;
    if perms.len() < 10 {
        return None; // `total 12` and anything unexpected
    }
    let kind = perms.chars().next()?;
    let (_links, _uid, _gid) = (next()?, next()?, next()?);
    let mut size = next()?;
    if size.ends_with(',') {
        // Devices show `major, minor` instead of a size.
        next()?;
        size = "0";
    }
    let (month, day, time) = (next()?, next()?, next()?);
    // The name follows a single space and may itself contain spaces.
    let mut name = rest.strip_prefix(' ')?;
    if kind == 'l'
        && let Some((link, _target)) = name.split_once(" -> ")
    {
        name = link;
    }
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(Entry {
        name: name.to_owned(),
        is_dir: kind == 'd',
        is_link: kind == 'l',
        size: size.parse().unwrap_or(0),
        mtime: ls_time(month, day, time, now).unwrap_or(0),
    })
}

/// Converts an `ls` date (`Jan  1 12:00` this year or `Jan  1  2023`) to unix seconds, read as UTC.
fn ls_time(month: &str, day: &str, time: &str, now: i64) -> Option<i64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = u32::try_from(MONTHS.iter().position(|m| *m == month)? + 1).ok()?;
    let day: u32 = day.parse().ok()?;
    if let Some((hour, minute)) = time.split_once(':') {
        let clock = hour.parse::<i64>().ok()? * 3600 + minute.parse::<i64>().ok()? * 60;
        let (year, _, _) = civil_from_days(now.div_euclid(86_400));
        let this_year = days_from_civil(year, month, day) * 86_400 + clock;
        // Recent dates show no year: a date in the future belongs to last year.
        Some(if this_year > now + 86_400 { days_from_civil(year - 1, month, day) * 86_400 + clock } else { this_year })
    } else {
        Some(days_from_civil(time.parse().ok()?, month, day) * 86_400)
    }
}

// ---------------------------------------------------------------------------
// Formatting

fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |secs| -secs),
    }
}

/// `YYYY-MM-DD HH:MM` in UTC.
fn format_time(secs: i64) -> String {
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let minutes = secs.rem_euclid(86_400) / 60;
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", minutes / 60, minutes % 60)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days.saturating_add(719_468);
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * shifted_month + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 }).unwrap_or(1);
    let year = year_of_era + era * 400;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Formats a byte count with binary units, e.g. `1.5 KiB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 { format!("{value:.1} {}", UNITS[unit]) } else { format!("{value:.0} {}", UNITS[unit]) }
}

/// Replaces control characters with visible placeholders.
fn visible_char(c: char) -> char {
    match c {
        '\0'..='\x1f' => char::from_u32(0x2400 + u32::from(c)).unwrap_or('\u{fffd}'),
        '\x7f' => '\u{2421}',
        '\u{80}'..='\u{9f}' => '\u{fffd}',
        c => c,
    }
}

fn sanitize(text: &str) -> String {
    text.chars().map(visible_char).collect()
}

/// Like [`sanitize`], keeping line breaks.
fn sanitize_lines(text: &str) -> String {
    text.chars().map(|c| if c == '\n' { c } else { visible_char(c) }).collect()
}

fn text_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Cuts `text` to `width` columns, ending with `…` when shortened.
fn truncate(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let kept = text.chars().take_while(|c| {
        used += c.width().unwrap_or(0);
        used < width
    });
    kept.chain(iter::once('…')).collect()
}

/// Cuts the start of `text` to `width` columns, starting with `…` when shortened.
fn truncate_start(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let mut kept: Vec<char> = text
        .chars()
        .rev()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used < width
        })
        .collect();
    kept.push('…');
    kept.into_iter().rev().collect()
}

/// Right-aligns `text` in `width` columns.
fn pad_start(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    format!("{}{text}", " ".repeat(width.saturating_sub(text_width(&text))))
}

/// Word-wraps `text` to `width` columns; words longer than a line are split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for source in text.lines() {
        let mut line = String::new();
        let mut used = 0;
        for word in source.split(' ').filter(|word| !word.is_empty()) {
            let word_width = text_width(word);
            if used > 0 && used + 1 + word_width <= width {
                line.push(' ');
                used += 1;
            } else if used > 0 {
                lines.push(mem::take(&mut line));
                used = 0;
            }
            for c in word.chars() {
                let w = c.width().unwrap_or(0);
                if used + w > width && used > 0 {
                    lines.push(mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += w;
            }
        }
        lines.push(line);
    }
    lines
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A unique scratch directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("kraal-files-{}-{name}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
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
        Entry { name: name.to_owned(), is_dir: false, is_link: false, size, mtime: 1_700_000_000 }
    }

    fn dir(name: &str) -> Entry {
        Entry { name: name.to_owned(), is_dir: true, is_link: false, size: 4096, mtime: 1_700_000_000 }
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
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
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
        let names =
            ["a b", "it's", "$HOME", "`id`", "é 漢字", r"back\slash", "-dash", "new\nline", "tab\there", "\"dq\""];
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
        let entry =
            |name: &str, is_dir, is_link, size, mtime| Entry { name: name.to_owned(), is_dir, is_link, size, mtime };
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
        assert_eq!(parse_listing(b"no marker", 0), None);
        assert_eq!(parse_listing(b"KRAAL:LIST\0/x\0?\0", 0), None);
        assert_eq!(parse_listing(b"KRAAL:LIST\0/x\0F\0", 0), Some(("/x".to_owned(), Vec::new())));
    }

    #[test]
    fn parses_ls_fallback() {
        let now = days_from_civil(2026, 10, 10) * 86_400;
        let listing = "total 12\n\
            drwxr-xr-x  2 1000 1000 4096 Jan  5 12:30 .\n\
            drwxr-xr-x 10 0    0    4096 Jan  5  2023 ..\n\
            -rw-r--r--  1 1000 1000  123 Mar  3  2024 name  with  spaces \n\
            lrwxrwxrwx  1 0    0       5 Feb  2 09:15 link -> target dir\n\
            lrwxrwxrwx  1 0    0       4 Feb  2 09:15 flink -> file\n\
            crw-rw-rw-  1 0    0    1,   3 Jan  1  2024 null\n\
            drwx------+ 3 0    0    4096 Dec 31 23:59 acl dir\n\
            garbage\n";
        let mut out = format!("KRAAL:LIST\0/srv\0L\0{listing}\0link\0").into_bytes();
        out.extend_from_slice(b"other\0");
        let (dir, entries) = parse_listing(&out, now).unwrap();
        assert_eq!(dir, "/srv");
        let entry =
            |name: &str, is_dir, is_link, size, mtime| Entry { name: name.to_owned(), is_dir, is_link, size, mtime };
        let day = |y, m, d| days_from_civil(y, m, d) * 86_400;
        assert_eq!(
            entries,
            [
                entry("name  with  spaces ", false, false, 123, day(2024, 3, 3)),
                entry("link", true, true, 5, day(2026, 2, 2) + 9 * 3600 + 15 * 60),
                entry("flink", false, true, 4, day(2026, 2, 2) + 9 * 3600 + 15 * 60),
                entry("null", false, false, 0, day(2024, 1, 1)),
                // A date later than now without a year is from last year.
                entry("acl dir", true, false, 4096, day(2025, 12, 31) + 23 * 3600 + 59 * 60),
            ]
        );
    }

    #[test]
    fn formats_times_and_sizes() {
        assert_eq!(format_time(0), "1970-01-01 00:00");
        assert_eq!(format_time(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(format_time(-60), "1969-12-31 23:59");
        assert_eq!(format_time(days_from_civil(2024, 2, 29) * 86_400 + 3_599), "2024-02-29 00:59");
        for days in [-800_000, -1, 0, 59, 11_016, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        // Extreme values do not panic.
        let _ = format_time(i64::MAX);
        let _ = format_time(i64::MIN);
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
    fn keys_map_to_actions() {
        let mut pane = sample();
        // Nothing acts on `..`.
        for key in [FileKey::Char('c'), FileKey::Char('m'), FileKey::Char('d'), FileKey::F(5), FileKey::Delete] {
            assert_eq!(pane.key(key), FilesAction::None, "{key:?}");
        }
        pane.key(FileKey::Down);
        let docs = local("/srv/kraal-test/left/Docs");
        let copy = FilesAction::Copy { sources: vec![docs.clone()], dest: local(RIGHT) };
        assert_eq!(pane.key(FileKey::Char('c')), copy);
        assert_eq!(pane.key(FileKey::F(5)), copy);
        assert_eq!(pane.key(FileKey::F(6)), FilesAction::Move { sources: vec![docs.clone()], dest: local(RIGHT) });
        assert_eq!(pane.key(FileKey::F(8)), FilesAction::Delete(vec![docs.clone()]));
        // Marks win over the cursor, in display order; a copy clears them.
        pane.key(FileKey::End);
        pane.key(FileKey::Char(' '));
        pane.key(FileKey::Up);
        pane.key(FileKey::Char(' '));
        let marked = vec![local("/srv/kraal-test/left/A.md"), local("/srv/kraal-test/left/b.txt")];
        assert_eq!(pane.key(FileKey::Char('m')), FilesAction::Move { sources: marked.clone(), dest: local(RIGHT) });
        assert_eq!(pane.key(FileKey::Char('d')), FilesAction::Delete(marked.clone()));
        assert_eq!(pane.key(FileKey::Delete), FilesAction::Delete(marked.clone()));
        assert_eq!(pane.key(FileKey::Char('c')), FilesAction::Copy { sources: marked, dest: local(RIGHT) });
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
        assert_eq!(pane.key(FileKey::Char('c')), FilesAction::Copy { sources, dest: local(RIGHT) });
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
        assert!(text[0].starts_with(&format!(" {LEFT}")), "{text:#?}");
        assert!(text[0].ends_with(&format!("│ {RIGHT}")), "{text:#?}");
        assert_eq!(buf[(0, 0)].bg, Color::LightBlue, "the active header is highlighted");
        assert_eq!(buf[(45, 0)].bg, Color::Indexed(237));
        assert_eq!(buf[(39, 5)].symbol(), "│");
        assert!(text[1].starts_with(" Name") && text[1].contains("Size") && text[1].contains("Modified (UTC)"));
        assert!(text[2].starts_with(" ..") && text[2].contains("UP--DIR"), "{text:#?}");
        assert_eq!(buf[(1, 2)].bg, Color::Cyan, "cursor on the active panel");
        assert!(text[3].starts_with(" Docs/") && text[3].contains("<DIR> 2023-11-14 22:13"), "{text:#?}");
        assert_eq!(buf[(1, 3)].fg, Color::LightBlue);
        assert!(text[5].starts_with(" A.md") && text[5].contains("1.0 KiB 2023-11-14 22:13"), "{text:#?}");
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
        assert_eq!(buf[(1, 3)].fg, Color::Yellow);
        assert!(text[10].starts_with(" 1 marked · 1 hidden"), "only a directory is marked: {text:#?}");
        assert_eq!(pane.visible_text(), text.join("\n"));
        // The inactive panel's cursor is subdued.
        pane.key(FileKey::Tab);
        let (buf, _) = render(&mut pane, 80, 12);
        assert_eq!(buf[(45, 0)].bg, Color::LightBlue);
        assert_eq!(buf[(1, 4)].bg, Color::Indexed(238));
    }

    #[test]
    fn narrow_areas_show_the_active_panel() {
        let mut pane = sample();
        let (_, text) = render(&mut pane, 20, 6);
        assert!(text[0].starts_with(" …"), "the header keeps the end of the path: {text:#?}");
        assert!(text[0].ends_with("left"), "{text:#?}");
        assert!(text[1].starts_with(" .."), "{text:#?}");
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
        assert!(text[0].starts_with(&format!(" {LEFT}  loading…")), "{text:#?}");
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
                for (col, row) in [(0, 0), (width / 2, height / 2), (width.saturating_sub(1), height.saturating_sub(1))]
                {
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
        assert_eq!(pane.panel(Side::Right).offset, 5);
        assert_eq!(pane.panel(Side::Right).cursor, 5, "the cursor stays visible");
        pane.scroll(60, 1000);
        assert_eq!(pane.panel(Side::Right).offset, 51 - 8);
        let (_, text) = render(&mut pane, 80, 12);
        assert!(text[9].ends_with("│ f49") || text[9].contains("│ f49 "), "{text:#?}");
        pane.scroll(60, -1000);
        assert_eq!(pane.panel(Side::Right).offset, 0);
        assert_eq!(pane.panel(Side::Right).cursor, 7);
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
        let script = fake_ssh(&tools, &format!("PATH={}; export PATH", shell_quote(bin.to_str().unwrap())));
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
}
