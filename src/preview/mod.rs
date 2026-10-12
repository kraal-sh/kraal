//! Rich file previews rendered into a ratatui [`Buffer`].
//!
//! A [`Preview`] shows a file or directory instead of a shell: rendered
//! Markdown, an image (drawn by the terminal when it shows images, with Unicode
//! half blocks otherwise), plain text with line numbers, a directory listing,
//! or a short note for binary files. Errors are rendered inside the area rather
//! than returned, so a preview never fails.

mod half_blocks;
mod load;
mod markdown;
mod spans;
/// `path` with its `.` and `..` worked out from the text alone.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};
use std::{fmt, fs};

use image::{ImageFormat, RgbaImage};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use self::half_blocks::{draw_half_blocks, fit_image};
use self::load::load;
use self::markdown::markdown_lines;
use self::spans::{expand_tabs, line_text, truncate_str, wrap_spans};
use crate::text::{human_size, sanitize, visible_char};

/// Largest prefix of a text or Markdown file that is loaded.
const MAX_TEXT_BYTES: u64 = 16 << 20;
/// Largest image file that is decoded.
const MAX_IMAGE_BYTES: u64 = 128 << 20;
/// How many leading bytes are checked for NUL when sniffing binary files.
const SNIFF_BYTES: usize = 8 << 10;
/// Decoded images are downscaled to fit this square to bound memory use.
const MAX_IMAGE_SIDE: u32 = 1024;
/// Tab stop width for text and code blocks.
const TAB_WIDTH: usize = 4;
/// Viewport assumed by [`Preview::visible_text`] before the first render.
const DEFAULT_VIEW: (u16, u16) = (80, 24);
/// Narrowest a table column is shrunk to before rows are simply truncated.
const MIN_COLUMN_WIDTH: usize = 3;

// The interface's colours where the preview draws its own chrome; content keeps fixed colours.
fn theme() -> crate::theme::Theme {
    crate::theme::current()
}
fn dim() -> Style {
    Style::new().fg(theme().dim)
}
fn note() -> Style {
    dim().add_modifier(Modifier::ITALIC)
}
fn error_style() -> Style {
    Style::new().fg(theme().production)
}
fn indicator() -> Style {
    dim()
}
fn gutter() -> Style {
    dim()
}
fn link_url() -> Style {
    dim()
}
fn quote_bar() -> Style {
    dim()
}
fn task_todo() -> Style {
    dim()
}
fn table_border() -> Style {
    dim()
}
fn rule() -> Style {
    dim()
}
fn bullet() -> Style {
    Style::new().fg(theme().accent)
}
const DIR_NAME: Style = Style::new().fg(Color::LightBlue).add_modifier(Modifier::BOLD);
const SYMLINK_NAME: Style = Style::new().fg(Color::LightCyan);
const EXEC_NAME: Style = Style::new().fg(Color::LightGreen);
const CODE_BLOCK: Style = Style::new().bg(Color::Indexed(236));
const CODE_LABEL: Style = CODE_BLOCK.fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
const INLINE_CODE: Style = Style::new().fg(Color::LightRed).bg(Color::Indexed(237));
const LINK: Style = Style::new().fg(Color::LightBlue).add_modifier(Modifier::UNDERLINED);
const IMAGE_ALT: Style = Style::new().fg(Color::Magenta).add_modifier(Modifier::ITALIC);
const QUOTE_TEXT: Style = Style::new().add_modifier(Modifier::ITALIC);
const TASK_DONE: Style = Style::new().fg(Color::Green);
const BULLETS: [&str; 4] = ["•", "◦", "▪", "▫"];

/// What a [`Preview`] shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreviewKind {
    /// Rendered Markdown (`.md`, `.markdown`).
    Markdown,
    /// A raster image: drawn by the terminal where it shows images, with half blocks elsewhere.
    Image,
    /// UTF-8 text with a line-number gutter.
    Text,
    /// A sorted directory listing.
    Directory,
    /// A file that is neither text nor a supported image.
    Binary,
}

/// How often the watcher looks at the file's date and size.
const WATCH_EVERY: Duration = Duration::from_secs(1);

/// A scrollable preview of one file or directory.
///
/// Nothing here touches the file system on the server's thread: the server reads the file with
/// [`Loaded::read`] on another thread when [`Preview::modified_since_load`] says so (after
/// opening, after `r`, after a change), and a watcher thread keeps the file's date and size.
pub struct Preview {
    path: PathBuf,
    kind: PreviewKind,
    content: Content,
    stamp: Option<Stamp>,
    /// The content is not read yet, or `r` asked to read it again.
    pending: bool,
    /// What the watcher last saw of the file (`None` until it looked).
    watch: Arc<Watch>,
    scroll: usize,
    /// Size of the last rendered area, if any.
    view: Option<(u16, u16)>,
    /// Laid-out lines for one width (all kinds except text and images).
    layout: Option<(u16, Vec<Line<'static>>)>,
    /// Image scaled for one area size.
    scaled: Option<((u16, u16), RgbaImage)>,
    /// What the panes' images share: set when the terminals may be sent the image itself.
    graphics: Option<Arc<crate::graphics::Shared>>,
    /// The image as terminals that show images get it.
    picture: Option<TerminalImage>,
    /// The text was given, not read from a file (see [`Preview::of_text`]).
    in_memory: bool,
}

/// An image preview as terminals that show images get it, under kraal's id for it.
struct TerminalImage {
    kid: u32,
    picture: Arc<crate::graphics::Picture>,
    /// The cells it takes, once shown, and how many times that changed.
    cells: Option<(u16, u16)>,
    generation: u64,
}

impl fmt::Debug for Preview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Preview")
            .field("path", &self.path)
            .field("kind", &self.kind)
            .field("scroll", &self.scroll)
            .field("view", &self.view)
            .finish_non_exhaustive()
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.watch.stop.store(true, Ordering::Relaxed);
        self.drop_picture();
    }
}

/// The file's date and size, as a watcher thread sees them.
#[derive(Default)]
struct Watch {
    seen: Mutex<Option<Option<Stamp>>>,
    stop: AtomicBool,
    /// Called when what the watcher sees changes (see [`Preview::on_change`]).
    changed: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Watch {
    /// Look at `path` every [`WATCH_EVERY`] until the preview is gone. A file system that
    /// hangs (a lost network mount) holds this thread, not the server.
    fn start(path: PathBuf) -> Arc<Watch> {
        let watch = Arc::new(Watch::default());
        let w = Arc::clone(&watch);
        let spawned = std::thread::Builder::new().name("kraal-preview".into()).spawn(move || {
            while !w.stop.load(Ordering::Relaxed) {
                let stamp = Stamp::of(&path);
                let before = w.seen.lock().unwrap_or_else(PoisonError::into_inner).replace(stamp);
                if before.is_some_and(|b| b != stamp)
                    && let Some(tell) = w.changed.lock().unwrap_or_else(PoisonError::into_inner).as_ref()
                {
                    tell();
                }
                std::thread::sleep(WATCH_EVERY);
            }
        });
        if spawned.is_err() {
            // Without a watcher, changes are not noticed; `r` still reads the file again.
            watch.stop.store(true, Ordering::Relaxed);
        }
        watch
    }
}

impl Preview {
    /// A preview of `path`, read later (see [`Preview::modified_since_load`]). Never fails:
    /// errors are rendered instead.
    pub fn open(path: &Path) -> Preview {
        Preview {
            path: path.to_path_buf(),
            kind: load::kind_from_extension(path).unwrap_or(PreviewKind::Text),
            content: Content::Loading,
            stamp: None,
            pending: true,
            watch: Watch::start(path.to_path_buf()),
            scroll: 0,
            view: None,
            layout: None,
            scaled: None,
            graphics: None,
            picture: None,
            in_memory: false,
        }
    }

    /// A preview of `text` (a paste buffer), titled `title`: there is no file to read or watch,
    /// and `r` changes nothing.
    pub fn of_text(title: &str, text: &str) -> Preview {
        let watch = Arc::new(Watch::default());
        watch.stop.store(true, Ordering::Relaxed);
        Preview {
            path: PathBuf::from(title),
            kind: PreviewKind::Text,
            content: Content::Text(TextData::new(text, None)),
            stamp: None,
            pending: false,
            watch,
            scroll: 0,
            view: None,
            layout: None,
            scaled: None,
            graphics: None,
            picture: None,
            in_memory: true,
        }
    }

    /// Shows text kraal holds rather than a file.
    pub fn in_memory(&self) -> bool {
        self.in_memory
    }

    /// Call `tell` (on the watcher's thread) when the file changes after it was read: the
    /// server reads it again then, instead of looking at it all the time.
    pub fn on_change(&self, tell: impl Fn() + Send + 'static) {
        *self.watch.changed.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(tell));
    }

    /// Show images as images (on terminals that can) rather than half blocks.
    pub fn set_graphics(&mut self, graphics: Arc<crate::graphics::Shared>) {
        self.graphics = Some(graphics);
    }

    /// The kind of content detected at the last load.
    pub fn kind(&self) -> PreviewKind {
        self.kind
    }

    /// A short title: the file or directory name. Worked out from the path alone (`..` and `.`
    /// included): drawing never waits on the file system.
    pub fn title(&self) -> String {
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
        name(&self.path)
            .or_else(|| std::path::absolute(&self.path).ok().map(|p| lexical(&p)).as_deref().and_then(name))
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// Asks for the file to be read again (by the server, off its thread), keeping the scroll
    /// position (clamped at next render).
    pub fn reload(&mut self) {
        self.pending = !self.in_memory;
    }

    /// Show a file read by [`Loaded::read`] (on another thread: reading and decoding can be slow).
    pub fn apply(&mut self, loaded: Loaded) {
        self.stamp = loaded.stamp;
        // What the watcher saw before this read is older; a later change it will see again.
        *self.watch.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(loaded.stamp);
        self.kind = loaded.kind;
        self.content = loaded.content;
        self.pending = false;
        self.layout = None;
        self.scaled = None;
        self.drop_picture();
        if let (Some(shared), Content::Image(image)) = (&self.graphics, &mut self.content)
            && let Some(png) = image.png.take()
        {
            let (width, height) = image.pixels.dimensions();
            // Beyond the memory kept for images, it is drawn with half blocks.
            self.picture = crate::graphics::Picture::within_quota(png, width, height, shared)
                .map(|picture| TerminalImage { kid: shared.new_id(), picture, cells: None, generation: 0 });
        }
    }

    /// The image goes; terminals that were sent it forget it.
    fn drop_picture(&mut self) {
        if self.picture.take().is_some_and(|p| p.cells.is_some())
            && let Some(shared) = &self.graphics
        {
            shared.changed();
        }
    }

    /// Draw the image in `area` as the placeholder cells of a terminal that shows images,
    /// centred in the pane's `size`; false when there is no such image.
    pub fn show_image(&mut self, area: Rect, size: (u16, u16), buf: &mut Buffer) -> bool {
        let (Some(shared), Some(picture)) = (&self.graphics, &mut self.picture) else { return false };
        if !matches!(self.content, Content::Image(_)) {
            return false;
        }
        let cells = fit_cells((picture.picture.width, picture.picture.height), size, shared.cell());
        if picture.cells != Some(cells) {
            picture.cells = Some(cells);
            picture.generation += 1;
            shared.changed();
        }
        for p in area.intersection(buf.area).positions() {
            buf[p].reset();
        }
        let at = (area.x + (size.0.saturating_sub(cells.0)) / 2, area.y + (size.1.saturating_sub(cells.1)) / 2);
        crate::graphics::write_placeholders(buf, area, at, picture.kid, cells.0, cells.1);
        true
    }

    /// The image shown as an image, by kraal's id: the generation of its placement, the
    /// picture and the placement.
    pub fn image(&self) -> Option<(u32, crate::graphics::Live)> {
        let p = self.picture.as_ref()?;
        let (cols, rows) = p.cells?;
        Some((p.kid, (p.generation, p.picture.clone(), vec![(0, cols, rows)])))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The Markdown source on screen, if the preview shows one (it may be older than the file).
    pub fn markdown_source(&self) -> Option<&str> {
        match &self.content {
            Content::Markdown(source, _) => Some(source),
            _ => None,
        }
    }

    /// Whether the file should be read: never read yet, asked again with `r`, or its
    /// modification time (or size) changed since the last read, as the watcher last saw it.
    pub fn modified_since_load(&self) -> bool {
        self.pending || self.watch.seen.lock().unwrap_or_else(PoisonError::into_inner).is_some_and(|s| s != self.stamp)
    }

    /// Fills `area` of `buf` with the preview. Never writes outside `area`.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        self.view = Some((area.width, area.height));
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].reset();
            }
        }
        if area.is_empty() {
            return;
        }
        if matches!(self.content, Content::Image(_)) {
            self.render_image(area, buf);
            return;
        }
        self.ensure_layout(area.width);
        let height = usize::from(area.height);
        let max_scroll = self.line_count(area.width).saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
        let lines = self.visible_lines(area.width, self.scroll, height);
        for (y, line) in (area.top()..area.bottom()).zip(&lines) {
            buf.set_line(area.x, y, line, area.width);
        }
        if let Some(percent) = self.scroll.saturating_mul(100).checked_div(max_scroll) {
            draw_scroll_indicator(area, buf, percent);
        }
    }

    /// Scrolls by `delta` lines (negative is up), clamped to the content.
    pub fn scroll(&mut self, delta: i32) {
        let amount = usize::try_from(delta.unsigned_abs()).unwrap_or(usize::MAX);
        self.scroll = if delta < 0 { self.scroll.saturating_sub(amount) } else { self.scroll.saturating_add(amount) };
        self.clamp_scroll();
    }

    /// Scrolls up by the last rendered height.
    pub fn page_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(self.page());
    }

    /// Scrolls down by the last rendered height.
    pub fn page_down(&mut self) {
        self.scroll = self.scroll.saturating_add(self.page());
        self.clamp_scroll();
    }

    /// Scrolls to the first line.
    pub fn top(&mut self) {
        self.scroll = 0;
    }

    /// Scrolls so that the last line is at the bottom of the area.
    pub fn bottom(&mut self) {
        self.scroll = usize::MAX;
        self.clamp_scroll();
    }

    /// Text of the visible lines, for the CLI `read` command and tests.
    ///
    /// Uses the last rendered size (80×24 before the first render). Images
    /// are summarised in one line instead of returning half-block glyphs.
    pub fn visible_text(&self) -> String {
        if let Content::Image(image) = &self.content {
            return image.summary();
        }
        let (width, height) = self.view.unwrap_or(DEFAULT_VIEW);
        if width == 0 || height == 0 {
            return String::new();
        }
        let height = usize::from(height);
        let scroll = self.scroll.min(self.line_count(width).saturating_sub(height));
        self.visible_lines(width, scroll, height)
            .iter()
            .map(|line| truncate_str(&line_text(line), usize::from(width)).trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn page(&self) -> usize {
        usize::from(self.view.map_or(DEFAULT_VIEW.1, |(_, h)| h).max(1))
    }

    fn clamp_scroll(&mut self) {
        let Some((width, height)) = self.view else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }
        self.ensure_layout(width);
        let max_scroll = self.line_count(width).saturating_sub(usize::from(height));
        self.scroll = self.scroll.min(max_scroll);
    }

    fn ensure_layout(&mut self, width: u16) {
        if matches!(self.content, Content::Text(_) | Content::Image(_))
            || self.layout.as_ref().is_some_and(|(w, _)| *w == width)
        {
            return;
        }
        self.layout = Some((width, self.build_lines(width)));
    }

    /// Lays out everything except text and images for `width` columns.
    fn build_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width).max(1);
        match &self.content {
            Content::Markdown(source, truncated_from) => {
                let mut lines = markdown_lines(source, width);
                if let Some(size) = truncated_from {
                    lines.push(Line::default());
                    lines.push(Line::styled(truncation_note(*size), note()));
                }
                lines
            }
            Content::Loading => vec![Line::styled("loading…", note())],
            Content::Directory(entries) => directory_lines(entries),
            Content::Binary(size) => {
                let mut message = format!("binary file, {size} bytes");
                if *size >= 1024 {
                    message.push_str(&format!(" ({})", human_size(*size)));
                }
                message_lines(&message, note(), width)
            }
            Content::Error(message) => message_lines(message, error_style(), width),
            Content::Text(_) | Content::Image(_) => Vec::new(),
        }
    }

    fn laid_out(&self, width: u16) -> Cow<'_, [Line<'static>]> {
        match &self.layout {
            Some((w, lines)) if *w == width => Cow::Borrowed(lines),
            _ => Cow::Owned(self.build_lines(width)),
        }
    }

    fn line_count(&self, width: u16) -> usize {
        match &self.content {
            Content::Text(text) => text.line_count(),
            Content::Image(_) => 0,
            _ => self.laid_out(width).len(),
        }
    }

    fn visible_lines(&self, width: u16, start: usize, count: usize) -> Vec<Line<'static>> {
        match &self.content {
            Content::Text(text) => {
                let end = text.line_count().min(start.saturating_add(count));
                (start..end).map(|i| text.line(i)).collect()
            }
            Content::Image(_) => Vec::new(),
            _ => self.laid_out(width).iter().skip(start).take(count).cloned().collect(),
        }
    }

    fn render_image(&mut self, area: Rect, buf: &mut Buffer) {
        let Content::Image(image) = &self.content else {
            return;
        };
        let key = (area.width, area.height);
        if self.scaled.as_ref().is_none_or(|(k, _)| *k != key) {
            let fitted = fit_image(&image.pixels, u32::from(area.width), u32::from(area.height) * 2);
            self.scaled = Some((key, fitted));
        }
        if let Some((_, pixels)) = &self.scaled {
            draw_half_blocks(pixels, area, buf);
        }
    }
}

/// Modification time and size of a file, used to detect changes.
/// A file read and decoded, ready for [`Preview::apply`].
pub struct Loaded {
    stamp: Option<Stamp>,
    kind: PreviewKind,
    content: Content,
}

impl Loaded {
    /// With `images`, an image is also kept as PNG, for the terminals that show images.
    pub fn read(path: &Path, images: bool) -> Loaded {
        // Stamp first, so a write racing with the read is seen as a change.
        let stamp = Stamp::of(path);
        // A decoder that panics on a malformed file gives an error to show: the preview is
        // never left waiting for a read that ended.
        let read = std::panic::catch_unwind(|| {
            let (kind, mut content) = load(path);
            if images && let Content::Image(image) = &mut content {
                let (width, height) = image.pixels.dimensions();
                image.png = crate::graphics::encode_png(width, height, image.pixels.as_raw()).ok();
            }
            (kind, content)
        });
        let (kind, content) = read.unwrap_or_else(|_| {
            let kind = load::kind_from_extension(path).unwrap_or(PreviewKind::Text);
            (kind, Content::Error(format!("cannot preview {}: reading it failed", path.display())))
        });
        Loaded { stamp, kind, content }
    }
}

/// The cells an image of `width × height` pixels takes in a pane of `size` cells of `cell`
/// pixels: as many as fit, its proportions kept.
fn fit_cells((width, height): (u32, u32), size: (u16, u16), cell: (u16, u16)) -> (u16, u16) {
    let (cw, ch) = (f64::from(cell.0.max(1)), f64::from(cell.1.max(1)));
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let scale = (f64::from(size.0) * cw / w).min(f64::from(size.1) * ch / h);
    let most = crate::graphics::DIACRITICS.len() as f64;
    let fit = |v: f64, max: u16| (v - 1e-6).ceil().clamp(1.0, f64::from(max.max(1)).min(most)) as u16;
    (fit(w * scale / cw, size.0), fit(h * scale / ch, size.1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(path: &Path) -> Option<Stamp> {
        let meta = fs::metadata(path).ok()?;
        Some(Stamp { modified: meta.modified().ok(), len: meta.len() })
    }
}

enum Content {
    /// Not read yet.
    Loading,
    /// The source, and the file's size when only its beginning was read.
    Markdown(String, Option<u64>),
    Image(ImageData),
    Text(TextData),
    Directory(Vec<DirEntry>),
    Binary(u64),
    Error(String),
}

struct ImageData {
    pixels: RgbaImage,
    width: u32,
    height: u32,
    format: Option<ImageFormat>,
    /// The pixels as PNG, when terminals may be sent the image.
    png: Option<Vec<u8>>,
}

impl ImageData {
    fn summary(&self) -> String {
        let format = self.format.and_then(|f| f.extensions_str().first().copied());
        match format {
            Some(ext) => format!("[image {}×{} {ext}]", self.width, self.height),
            None => format!("[image {}×{}]", self.width, self.height),
        }
    }
}

struct TextData {
    /// Lines with tabs expanded and control characters made visible.
    lines: Vec<String>,
    gutter: usize,
    /// Full file size when only a prefix was loaded.
    truncated_from: Option<u64>,
}

impl TextData {
    fn new(text: &str, truncated_from: Option<u64>) -> TextData {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let lines: Vec<String> = text.lines().map(expand_tabs).collect();
        let gutter = lines.len().to_string().len().max(3);
        TextData { lines, gutter, truncated_from }
    }

    fn line_count(&self) -> usize {
        self.lines.len().max(1) + usize::from(self.truncated_from.is_some())
    }

    fn line(&self, index: usize) -> Line<'static> {
        match self.lines.get(index) {
            Some(text) => Line::from(vec![
                Span::styled(format!("{:>w$} │ ", index + 1, w = self.gutter), gutter()),
                Span::raw(text.clone()),
            ]),
            None if self.lines.is_empty() && index == 0 => Line::styled("(empty file)", note()),
            None => Line::styled(truncation_note(self.truncated_from.unwrap_or_default()), note()),
        }
    }
}

/// The line that ends a file shown only in part.
fn truncation_note(size: u64) -> String {
    format!("… truncated, file is {} ({size} bytes)", human_size(size))
}

struct DirEntry {
    name: String,
    is_dir: bool,
    size: u64,
    hidden: bool,
    executable: bool,
    link_target: Option<String>,
}

// ---------------------------------------------------------------------------
// Simple layouts

fn directory_lines(entries: &[DirEntry]) -> Vec<Line<'static>> {
    if entries.is_empty() {
        return vec![Line::styled("(empty directory)", note())];
    }
    entries
        .iter()
        .map(|entry| {
            let size = if entry.is_dir { String::new() } else { human_size(entry.size) };
            let mut name_style = if entry.is_dir {
                DIR_NAME
            } else if entry.link_target.is_some() {
                SYMLINK_NAME
            } else if entry.executable {
                EXEC_NAME
            } else {
                Style::new()
            };
            let mut size_style = Style::new();
            if entry.hidden {
                name_style = name_style.add_modifier(Modifier::DIM);
                size_style = size_style.add_modifier(Modifier::DIM);
            }
            let mut name = sanitize(&entry.name);
            if entry.is_dir {
                name.push('/');
            }
            let mut spans = vec![Span::styled(format!("{size:>9}  "), size_style), Span::styled(name, name_style)];
            if let Some(target) = &entry.link_target {
                spans.push(Span::styled(format!(" → {}", sanitize(target)), dim()));
            }
            Line::from(spans)
        })
        .collect()
}

fn message_lines(message: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let text: String = message.chars().map(|c| if c == '\n' { c } else { visible_char(c) }).collect();
    wrap_spans(&[Span::styled(text, style)], width).into_iter().map(Line::from).collect()
}

fn draw_scroll_indicator(area: Rect, buf: &mut Buffer, percent: usize) {
    let label = format!(" {percent}% ");
    let Ok(width) = u16::try_from(label.len()) else {
        return;
    };
    if area.width < width || area.height == 0 {
        return;
    }
    let x = area.right() - width;
    let y = area.bottom() - 1;
    for dx in 0..width {
        buf[(x + dx, y)].reset();
    }
    buf.set_stringn(x, y, &label, usize::from(width), indicator());
}
