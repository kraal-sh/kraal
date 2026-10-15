//! A two-panel file manager pane in the style of Midnight Commander.
//!
//! Each panel shows a local directory or a directory on a server reached over
//! SSH. Listings run on background threads and are applied by
//! [`FilesPane::poll`], so the pane never blocks the server. File operations
//! are not performed here: keys return [`FilesAction`]s that the server
//! carries out (with `kraal cp` semantics) before calling
//! [`FilesPane::refresh`]. Errors are rendered inside the panels; nothing
//! panics or draws outside the given area, whatever the input or size.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::mem;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

#[cfg(test)]
mod fuzz;
mod listing;
#[cfg(test)]
mod tests;
mod view;

pub(crate) use listing::remote_dir_arg;
use listing::{Listing, Ssh, home_dir, list};
use view::{Geometry, buffer_rows, footer_layout, geometry};

/// Viewport assumed before the first render.
const DEFAULT_VIEW: (u16, u16) = (80, 24);

/// Where a panel points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A directory on this machine.
    Local(PathBuf),
    /// A directory on a server, listed over SSH.
    Remote(Remote),
    Laptop {
        owner_id: String,
        host: String,
        path: String,
    },
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
    pub fn saved(&self) -> String {
        if matches!(self, Self::Laptop { .. }) { "~".into() } else { self.display() }
    }

    fn title(&self) -> String {
        if let Self::Laptop { host, .. } = self { host.clone() } else { self.short_display() }
    }

    /// The child entry `name` of this directory.
    pub fn join(&self, name: &str) -> Location {
        match self {
            Location::Laptop { owner_id, host, path } => {
                Location::Laptop { owner_id: owner_id.clone(), host: host.clone(), path: join_remote(path, name) }
            }
            Location::Local(path) => Location::Local(path.join(name)),
            Location::Remote(remote) => Location::Remote(remote.with_path(join_remote(&remote.path, name))),
        }
    }

    /// The parent directory, or `None` at the root (and at an unresolved `~`).
    fn parent(&self) -> Option<Location> {
        match self {
            Location::Laptop { owner_id, host, path } => parent_remote(path).map(|path| Location::Laptop {
                owner_id: owner_id.clone(),
                host: host.clone(),
                path,
            }),
            Location::Local(path) => {
                path.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| Location::Local(p.to_path_buf()))
            }
            Location::Remote(remote) => {
                parent_remote(&remote.path).map(|path| Location::Remote(remote.with_path(path)))
            }
        }
    }

    /// "edge-proxy:/var/log" or "/home/me": the syntax of `kraal cp`.
    pub fn display(&self) -> String {
        match self {
            Location::Local(path) => path.display().to_string(),
            Location::Remote(remote) => format!("{}:{}", remote.server, remote.path),
            Location::Laptop { host, path, .. } => format!("{host}:{path}"),
        }
    }

    /// [`Location::display`], unless a local path is not valid UTF-8: shown approximately, it
    /// could name another file, so nothing is done with it.
    pub fn exact_display(&self) -> Option<String> {
        match self {
            Location::Local(path) => path.to_str().map(str::to_owned),
            Location::Remote(_) => Some(self.display()),
            Location::Laptop { .. } => None,
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
            Location::Remote(_) | Location::Laptop { .. } => self.display(),
        }
    }

    /// The last path component: this directory's name in its parent.
    pub fn name(&self) -> Option<String> {
        match self {
            Location::Local(path) => path.file_name().map(|n| n.to_string_lossy().into_owned()),
            Location::Remote(Remote { path, .. }) | Location::Laptop { path, .. } => {
                let trimmed = path.trim_end_matches('/');
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Entry {
    pub(crate) name: String,
    /// A directory, or a symbolic link to one.
    pub(crate) is_dir: bool,
    pub(crate) is_link: bool,
    pub(crate) size: u64,
    /// unix seconds
    pub(crate) mtime: i64,
    /// `name` is the entry's real name. A name that is not UTF-8 is shown approximately, and
    /// nothing is done to such an entry: an approximate name could designate another file.
    pub(crate) exact: bool,
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
        /// Names already in the destination, which the copy replaces.
        replaces: Vec<String>,
    },
    /// Same, but move (the server asks for confirmation first).
    Move {
        sources: Vec<Location>,
        dest: Location,
        replaces: Vec<String>,
    },
    /// Delete (the server asks for confirmation first).
    Delete(Vec<Location>),
    /// Open a local file in a preview pane (Enter / F3 on a local file).
    Preview(PathBuf),
    /// Open the file under the cursor in the local editor (key `e`, F4); a server's file is
    /// saved back to it.
    Edit(Location),
    /// Open an SSH or local shell pane in this directory (key `s`).
    Shell(Location),
    /// Let the user pick a server for a panel (key `o` or `:`); the server answers with `set_location`.
    PickLocation(Side),
    Close,
    /// Nothing was done, and here is why (shown to the user).
    Refused(String),
}

const INEXACT: &str = "a name here is not valid UTF-8: kraal cannot point at it safely; use a shell (key s)";

/// One of the two panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// A listing finished by a background thread.
struct Done {
    side: Side,
    request: u64,
    result: Listing,
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
    owner: Option<u32>,
    pending: bool,
    laptop_bytes: Vec<u8>,
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
    /// First row shown, per client: each keeps its own view of the shared panel, and a click
    /// counts in what its client shows (a shorter one scrolls further to keep the cursor in
    /// sight). A client's entry goes when it detaches.
    offsets: HashMap<u32, usize>,
    /// Id of the latest listing request; older results are dropped.
    request: u64,
    cancel: Arc<AtomicBool>,
    /// Entry to put the cursor on when the pending listing arrives.
    select: Option<String>,
}

impl Panel {
    fn new(location: Location) -> Panel {
        Panel {
            owner: None,
            pending: false,
            laptop_bytes: Vec::new(),
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
            offsets: HashMap::new(),
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
        self.laptop_bytes.clear();
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
        self.offsets.clear();
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

    /// Remember what client `viewer` shows.
    fn set_offset(&mut self, viewer: u32, offset: usize) {
        self.offsets.insert(viewer, offset);
    }

    fn move_by(&mut self, delta: isize) {
        let target = self.cursor.saturating_add_signed(delta);
        self.cursor = target.min(self.rows_len().saturating_sub(1));
    }

    /// The first row client `viewer` shows in a list of `height` rows, the cursor in sight.
    fn scroll_offset(&self, viewer: u32, height: usize) -> usize {
        let rows = self.rows_len();
        let mut offset = self.offsets.get(&viewer).copied().unwrap_or(0);
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
    /// The client drawing the pane, or whose key or click is being handled: offsets are its own.
    viewer: u32,
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
            viewer: 0,
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
        format!("files · {} ⇄ {}", left.location.title(), right.location.title())
    }

    pub fn location(&self, side: Side) -> &Location {
        &self.panel(side).location
    }

    /// Points a panel at `loc` and lists it.
    pub fn set_location(&mut self, side: Side, loc: Location) {
        self.panel_mut(side).owner = None;
        self.navigate(side, loc, None);
    }

    fn restricted(&self, side: Side) -> Option<String> {
        let panel = self.panel(side);
        match &panel.location {
            Location::Laptop { host, .. } if panel.owner != Some(self.viewer) => {
                Some(format!("{host}: shown on that machine only"))
            }
            _ => None,
        }
    }

    pub fn can_set(&self, side: Side, client: u32) -> bool {
        let panel = self.panel(side);
        !matches!(panel.location, Location::Laptop { .. }) || panel.owner.is_none_or(|owner| owner == client)
    }

    pub fn set_laptop(&mut self, side: Side, location: Location, client: u32) -> bool {
        if !self.can_set(side, client) || matches!(self.panel(side.other()).location, Location::Laptop { .. }) {
            return false;
        }
        self.panel_mut(side).owner = Some(client);
        self.navigate(side, location, None);
        true
    }

    pub fn reconnect_laptop(&mut self, owner_id: &str, client: u32) {
        for side in [Side::Left, Side::Right] {
            if matches!(&self.panel(side).location, Location::Laptop { owner_id: owner, .. } if owner == owner_id) {
                self.panel_mut(side).owner = Some(client);
                self.load(side);
            }
        }
    }

    pub fn laptop_requests(&mut self) -> Vec<(u32, Side, u64, String)> {
        let mut requests = Vec::new();
        for side in [Side::Left, Side::Right] {
            let panel = self.panel_mut(side);
            if !std::mem::take(&mut panel.pending) {
                continue;
            }
            if let Location::Laptop { path, host, .. } = &panel.location {
                if let Some(owner) = panel.owner {
                    requests.push((owner, side, panel.request, path.clone()));
                } else {
                    let error = format!("{host} is not attached: press o to choose a machine");
                    panel.apply(Err(error), false);
                }
            }
        }
        requests
    }

    pub fn laptop_chunk(&mut self, client: u32, side: Side, request: u64, data: Vec<u8>, done: bool) {
        let panel = self.panel_mut(side);
        if panel.owner != Some(client)
            || panel.request != request
            || !panel.loading
            || !matches!(panel.location, Location::Laptop { .. })
        {
            return;
        }
        if data.len() > crate::protocol::LAPTOP_CHUNK
            || panel.laptop_bytes.len() + data.len() > crate::protocol::LAPTOP_LIST_LIMIT
        {
            self.laptop_result(client, side, request, Err("Laptop listing is too large".into()));
            return;
        }
        panel.laptop_bytes.extend(data);
        if done {
            let result =
                serde_json::from_slice(&panel.laptop_bytes).unwrap_or_else(|_| Err("Invalid laptop listing".into()));
            self.laptop_result(client, side, request, result);
        }
    }

    pub fn laptop_result(
        &mut self,
        client: u32,
        side: Side,
        request: u64,
        result: Result<(String, Vec<Entry>), String>,
    ) {
        let hidden = self.show_hidden;
        let panel = self.panel_mut(side);
        if panel.owner != Some(client) || panel.request != request || !panel.loading {
            return;
        }
        let Location::Laptop { owner_id, host, .. } = &panel.location else {
            return;
        };
        panel.laptop_bytes.clear();
        let result = result.map(|(path, entries)| {
            (Location::Laptop { owner_id: owner_id.clone(), host: host.clone(), path }, entries)
        });
        panel.apply(result, hidden);
    }

    /// Reloads session and SSH panels after a job finishes.
    pub fn refresh(&mut self) {
        for side in [Side::Left, Side::Right] {
            if !matches!(self.panel(side).location, Location::Laptop { .. }) {
                self.load(side);
            }
        }
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
        let viewer = self.viewer;
        for panel in &mut self.panels {
            let offset = panel.scroll_offset(viewer, height);
            panel.set_offset(viewer, offset);
        }
        self.draw(area, buf, false);
    }

    /// The client about to draw the pane: what it shows is remembered as its own.
    pub fn set_viewer(&mut self, viewer: u32) {
        self.viewer = viewer;
    }

    /// A client went away: what it showed is forgotten.
    pub fn forget_viewer(&mut self, viewer: u32) -> bool {
        let mut changed = false;
        for panel in &mut self.panels {
            panel.offsets.remove(&viewer);
            if panel.owner == Some(viewer) {
                changed = true;
                panel.owner = None;
                panel.pending = false;
                panel.supersede();
                if let Location::Laptop { host, .. } = &panel.location {
                    panel.apply(Err(format!("{host} is not attached: press o to choose a machine")), self.show_hidden);
                }
            }
        }
        changed
    }

    /// Text of what is shown, for tests and the CLI `read` command.
    ///
    /// Uses the last rendered size (80×24 before the first render).
    pub fn visible_text(&self) -> String {
        let (width, height) = self.view.unwrap_or(DEFAULT_VIEW);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        self.draw(area, &mut buf, true);
        buffer_rows(&buf, area).join("\n")
    }

    /// Handles a key; the returned action is for the server to carry out.
    pub fn key(&mut self, key: FileKey) -> FilesAction {
        let side = self.active;
        if let Some(why) = self.restricted(side) {
            return match key {
                FileKey::Tab => {
                    self.active = side.other();
                    FilesAction::None
                }
                FileKey::Esc | FileKey::Char('q') | FileKey::F(10) => FilesAction::Close,
                FileKey::Char('o' | ':') if self.panel(side).owner.is_none() => FilesAction::PickLocation(side),
                _ => FilesAction::Refused(why),
            };
        }
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
        let laptop = matches!(self.panel(side).location, Location::Laptop { .. });
        if laptop && matches!(key, FileKey::Char('e' | 's') | FileKey::F(4)) {
            return FilesAction::Refused(
                "Editing and shells are unavailable on the laptop: open them on that machine".into(),
            );
        }
        if (laptop && matches!(key, FileKey::Char('d') | FileKey::Delete | FileKey::F(8)))
            || (self.panels.iter().any(|p| matches!(p.location, Location::Laptop { .. }))
                && matches!(key, FileKey::Char('m') | FileKey::F(6)))
        {
            return FilesAction::Refused(
                "Laptop move and delete: not yet available. Use kraal cp on the laptop to copy files".into(),
            );
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
                return match self.sources(side) {
                    Ok(sources) if sources.is_empty() => FilesAction::None,
                    Ok(sources) => FilesAction::Delete(sources),
                    Err(why) => FilesAction::Refused(why),
                };
            }
            FileKey::Char('e') | FileKey::F(4) => return self.edit(side),
            FileKey::Char('s') => return FilesAction::Shell(self.panel(side).location.clone()),
            FileKey::Char('o' | ':') => return FilesAction::PickLocation(side),
            FileKey::Char('r') => {
                for side in [Side::Left, Side::Right] {
                    if self.restricted(side).is_none() {
                        self.load(side);
                    }
                }
            }
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
        if let Some(why) = self.restricted(side) {
            return FilesAction::Refused(why);
        }
        let viewer = self.viewer;
        let panel = self.panel_mut(side);
        let index = panel.scroll_offset(viewer, usize::from(list_height)).saturating_add(usize::from(row - list_y));
        if index >= panel.rows_len() {
            return FilesAction::None;
        }
        panel.cursor = index;
        if double { self.open(side) } else { FilesAction::None }
    }

    /// The client the next key or click comes from, and the size it is counted in: what that
    /// client shows of the pane (clients with smaller terminals show less of it).
    pub fn set_view(&mut self, viewer: u32, width: u16, height: u16) {
        self.viewer = viewer;
        self.view = Some((width, height));
    }

    /// Scrolls the active panel by `delta` rows (negative is up): keyboard scrolling.
    pub fn scroll_active(&mut self, delta: i32) {
        let side = self.active;
        self.scroll_side(side, delta);
    }

    /// Scrolls the panel under column `col` by `delta` rows (negative is up).
    /// The cursor is kept within the visible rows.
    pub fn scroll(&mut self, col: u16, delta: i32) {
        let side = self.geometry().side_at(col).unwrap_or(self.active);
        self.scroll_side(side, delta);
    }

    fn scroll_side(&mut self, side: Side, delta: i32) {
        if self.restricted(side).is_some() {
            return;
        }
        let height = usize::from(self.geometry().list.1).max(1);
        let viewer = self.viewer;
        let panel = self.panel_mut(side);
        let rows = panel.rows_len();
        let max_offset = rows.saturating_sub(height);
        let delta = isize::try_from(delta).unwrap_or(0);
        let offset = panel.scroll_offset(viewer, height).saturating_add_signed(delta).min(max_offset);
        panel.set_offset(viewer, offset);
        let last_visible = (offset + height - 1).min(rows.saturating_sub(1));
        panel.cursor = panel.cursor.clamp(offset.min(last_visible), last_visible);
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
        if matches!(panel.location, Location::Laptop { .. }) {
            panel.entries.clear();
            panel.shown.clear();
            panel.marked.clear();
            panel.pending = true;
            (self.wake)();
            return;
        }
        let location = panel.location.clone();
        let (done, wake, ssh) = (Arc::clone(&self.done), Arc::clone(&self.wake), self.ssh.clone());
        let spawned = crate::threads::named("kraal-files").spawn(move || {
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
            Some(Row::Entry(entry)) if !entry.exact => return FilesAction::Refused(INEXACT.into()),
            Some(Row::Entry(entry)) if entry.is_dir => {
                let target = panel.location.join(&entry.name);
                self.navigate(side, target, None);
            }
            Some(Row::Entry(entry)) => {
                if matches!(panel.location, Location::Laptop { .. }) {
                    return FilesAction::Refused(
                        "Preview is unavailable on the laptop: open the file on that machine".into(),
                    );
                }
                if let Location::Local(path) = panel.location.join(&entry.name) {
                    return FilesAction::Preview(path);
                }
            }
            None => {}
        }
        FilesAction::None
    }

    /// `e`: the file under the cursor, to edit.
    fn edit(&self, side: Side) -> FilesAction {
        let panel = self.panel(side);
        match panel.current() {
            None => FilesAction::None,
            Some(entry) if !entry.exact => FilesAction::Refused(INEXACT.into()),
            Some(entry) if entry.is_dir => {
                FilesAction::Refused(format!("{} is a directory: e edits a file", entry.name))
            }
            Some(entry) => FilesAction::Edit(panel.location.join(&entry.name)),
        }
    }

    /// Goes to the parent directory, with the cursor on the directory left.
    fn go_up(&mut self, side: Side) {
        let location = &self.panel(side).location;
        if let Some(parent) = location.parent() {
            let name = location.name();
            self.navigate(side, parent, name);
        }
    }

    /// The marked entries, or else the one under the cursor; never `..`. Refused when one of
    /// them has a name kraal cannot represent exactly.
    fn sources(&self, side: Side) -> Result<Vec<Location>, String> {
        let panel = self.panel(side);
        let marked = panel.marked_entries();
        let entries = if marked.is_empty() { panel.current().into_iter().collect() } else { marked };
        if entries.iter().any(|e| !e.exact) {
            return Err(INEXACT.into());
        }
        Ok(entries.into_iter().map(|e| panel.location.join(&e.name)).collect())
    }

    fn transfer(&mut self, side: Side, is_move: bool) -> FilesAction {
        if matches!(self.panel(side.other()).location, Location::Laptop { .. }) {
            return FilesAction::Refused(
                "Copying into the laptop: not yet available. Run kraal cp on the laptop".into(),
            );
        }
        if matches!(self.panel(side).location, Location::Laptop { .. })
            && matches!(self.panel(side.other()).location, Location::Remote(_))
        {
            return FilesAction::Refused("Copy to the session's machine first, or run kraal cp on the laptop".into());
        }
        let sources = match self.sources(side) {
            Ok(s) => s,
            Err(why) => return FilesAction::Refused(why),
        };
        let target = self.panel(side.other());
        let dest = target.location.clone();
        if sources.is_empty() || dest == self.panel(side).location {
            return FilesAction::None;
        }
        // What the other panel lists is what the copy would overwrite.
        let replaces: Vec<String> = sources
            .iter()
            .filter_map(Location::name)
            .filter(|n| !target.loading && target.entries.iter().any(|e| &e.name == n))
            .collect();
        if is_move {
            FilesAction::Move { sources, dest, replaces }
        } else {
            // Marks are dropped once a copy is under way; moved or deleted
            // entries lose theirs when the listing is refreshed.
            self.panel_mut(side).marked.clear();
            FilesAction::Copy { sources, dest, replaces }
        }
    }

    // -----------------------------------------------------------------------
    // Drawing
}
