//! Rich file previews rendered into a ratatui [`Buffer`].
//!
//! A [`Preview`] shows a file or directory instead of a shell: rendered
//! Markdown, an image drawn with Unicode half blocks, plain text with line
//! numbers, a directory listing, or a short note for binary files. Errors are
//! rendered inside the area rather than returned, so a preview never fails.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::mem;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, bail};
use image::imageops::{self, FilterType};
use image::{ImageFormat, ImageReader, Rgba, RgbaImage};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

const DIM: Style = Style::new().fg(Color::DarkGray);
const NOTE: Style = DIM.add_modifier(Modifier::ITALIC);
const ERROR: Style = Style::new().fg(Color::Red);
const INDICATOR: Style = DIM;
const GUTTER: Style = DIM;
const DIR_NAME: Style = Style::new().fg(Color::LightBlue).add_modifier(Modifier::BOLD);
const SYMLINK_NAME: Style = Style::new().fg(Color::LightCyan);
const EXEC_NAME: Style = Style::new().fg(Color::LightGreen);
const CODE_BLOCK: Style = Style::new().bg(Color::Indexed(236));
const CODE_LABEL: Style = CODE_BLOCK.fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
const INLINE_CODE: Style = Style::new().fg(Color::LightRed).bg(Color::Indexed(237));
const LINK: Style = Style::new().fg(Color::LightBlue).add_modifier(Modifier::UNDERLINED);
const LINK_URL: Style = DIM;
const IMAGE_ALT: Style = Style::new().fg(Color::Magenta).add_modifier(Modifier::ITALIC);
const QUOTE_BAR: Style = DIM;
const QUOTE_TEXT: Style = Style::new().add_modifier(Modifier::ITALIC);
const BULLET: Style = Style::new().fg(Color::Yellow);
const TASK_DONE: Style = Style::new().fg(Color::Green);
const TASK_TODO: Style = DIM;
const TABLE_BORDER: Style = DIM;
const RULE: Style = DIM;
const BULLETS: [&str; 4] = ["•", "◦", "▪", "▫"];

/// What a [`Preview`] shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreviewKind {
    /// Rendered Markdown (`.md`, `.markdown`).
    Markdown,
    /// A raster image drawn with half blocks.
    Image,
    /// UTF-8 text with a line-number gutter.
    Text,
    /// A sorted directory listing.
    Directory,
    /// A file that is neither text nor a supported image.
    Binary,
}

/// A scrollable preview of one file or directory.
pub struct Preview {
    path: PathBuf,
    kind: PreviewKind,
    content: Content,
    stamp: Option<Stamp>,
    scroll: usize,
    /// Size of the last rendered area, if any.
    view: Option<(u16, u16)>,
    /// Laid-out lines for one width (all kinds except text and images).
    layout: Option<(u16, Vec<Line<'static>>)>,
    /// Image scaled for one area size.
    scaled: Option<((u16, u16), RgbaImage)>,
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

impl Preview {
    /// Opens a preview of `path`. Never fails: errors are rendered instead.
    pub fn open(path: &Path) -> Preview {
        let mut preview = Preview {
            path: path.to_path_buf(),
            kind: PreviewKind::Text,
            content: Content::Error(String::new()),
            stamp: None,
            scroll: 0,
            view: None,
            layout: None,
            scaled: None,
        };
        preview.reload();
        preview
    }

    /// The kind of content detected at the last load.
    pub fn kind(&self) -> PreviewKind {
        self.kind
    }

    /// A short title: the file or directory name.
    pub fn title(&self) -> String {
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
        name(&self.path)
            .or_else(|| fs::canonicalize(&self.path).ok().as_deref().and_then(name))
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// Re-reads the file, keeping the scroll position (clamped at next render).
    pub fn reload(&mut self) {
        // Stamp first, so a write racing with the read is seen as a change.
        self.stamp = Stamp::of(&self.path);
        let (kind, content) = load(&self.path);
        self.kind = kind;
        self.content = content;
        self.layout = None;
        self.scaled = None;
    }

    /// Whether the file's modification time (or size) changed since the last load.
    pub fn modified_since_load(&self) -> bool {
        Stamp::of(&self.path) != self.stamp
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
            Content::Markdown(source) => markdown_lines(source, width),
            Content::Directory(entries) => directory_lines(entries),
            Content::Binary(size) => {
                let mut message = format!("binary file, {size} bytes");
                if *size >= 1024 {
                    message.push_str(&format!(" ({})", human_size(*size)));
                }
                message_lines(&message, NOTE, width)
            }
            Content::Error(message) => message_lines(message, ERROR, width),
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
    Markdown(String),
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
                Span::styled(format!("{:>w$} │ ", index + 1, w = self.gutter), GUTTER),
                Span::raw(text.clone()),
            ]),
            None if self.lines.is_empty() && index == 0 => Line::styled("(empty file)", NOTE),
            None => {
                let size = self.truncated_from.unwrap_or_default();
                Line::styled(format!("… truncated, file is {} ({size} bytes)", human_size(size)), NOTE)
            }
        }
    }
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
// Loading

fn load(path: &Path) -> (PreviewKind, Content) {
    let ext_kind = kind_from_extension(path);
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            let message = format!("cannot read {}: {err}", path.display());
            return (ext_kind.unwrap_or(PreviewKind::Text), Content::Error(message));
        }
    };
    if meta.is_dir() {
        let content = read_directory(path).map_or_else(error_content, Content::Directory);
        return (PreviewKind::Directory, content);
    }
    if !meta.is_file() {
        let message = format!("cannot preview {}: not a regular file", path.display());
        return (ext_kind.unwrap_or(PreviewKind::Binary), Content::Error(message));
    }
    match ext_kind {
        Some(PreviewKind::Markdown) => {
            let content = read_capped(path, MAX_TEXT_BYTES).map_or_else(error_content, |(bytes, _)| {
                Content::Markdown(String::from_utf8_lossy(&bytes).into_owned())
            });
            (PreviewKind::Markdown, content)
        }
        Some(PreviewKind::Image) => (PreviewKind::Image, load_image(path)),
        _ => sniff(path, meta.len()),
    }
}

fn kind_from_extension(path: &Path) -> Option<PreviewKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" => Some(PreviewKind::Markdown),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Some(PreviewKind::Image),
        _ => None,
    }
}

/// Classifies a file without a known extension by its content.
fn sniff(path: &Path, size: u64) -> (PreviewKind, Content) {
    let (bytes, truncated) = match read_capped(path, MAX_TEXT_BYTES) {
        Ok(read) => read,
        Err(err) => return (PreviewKind::Text, error_content(err)),
    };
    let size = size.max(bytes.len() as u64);
    if is_supported_image(&bytes) {
        return (PreviewKind::Image, load_image(path));
    }
    if bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) {
        return (PreviewKind::Binary, Content::Binary(size));
    }
    match decode_utf8(bytes, truncated) {
        Some(text) => {
            let data = TextData::new(&text, truncated.then_some(size));
            (PreviewKind::Text, Content::Text(data))
        }
        None => (PreviewKind::Binary, Content::Binary(size)),
    }
}

fn is_supported_image(bytes: &[u8]) -> bool {
    matches!(
        image::guess_format(bytes),
        Ok(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP | ImageFormat::Bmp)
    )
}

/// Decodes UTF-8, tolerating a sequence cut short by the read limit.
fn decode_utf8(bytes: Vec<u8>, truncated: bool) -> Option<String> {
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(err) => {
            let utf8 = err.utf8_error();
            if !truncated || utf8.error_len().is_some() {
                return None;
            }
            let mut bytes = err.into_bytes();
            bytes.truncate(utf8.valid_up_to());
            String::from_utf8(bytes).ok()
        }
    }
}

/// Reads at most `cap` bytes; the flag tells whether the file was longer.
fn read_capped(path: &Path, cap: u64) -> anyhow::Result<(Vec<u8>, bool)> {
    let context = || format!("cannot read {}", path.display());
    let file = File::open(path).with_context(context)?;
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes).with_context(context)?;
    let truncated = bytes.len() as u64 > cap;
    if truncated {
        bytes.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
    }
    Ok((bytes, truncated))
}

fn error_content(err: anyhow::Error) -> Content {
    Content::Error(format!("{err:#}"))
}

fn load_image(path: &Path) -> Content {
    decode_image(path).map_or_else(error_content, Content::Image)
}

fn decode_image(path: &Path) -> anyhow::Result<ImageData> {
    let (bytes, truncated) = read_capped(path, MAX_IMAGE_BYTES)?;
    if truncated {
        bail!("cannot preview {}: image larger than {}", path.display(), human_size(MAX_IMAGE_BYTES));
    }
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let format = reader.format();
    let decoded = reader.decode().with_context(|| format!("cannot decode image {}", path.display()))?;
    let (width, height) = (decoded.width(), decoded.height());
    let decoded = if width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        decoded.thumbnail(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE)
    } else {
        decoded
    };
    Ok(ImageData { pixels: decoded.into_rgba8(), width, height, format })
}

fn read_directory(path: &Path) -> anyhow::Result<Vec<DirEntry>> {
    let reader = fs::read_dir(path).with_context(|| format!("cannot list {}", path.display()))?;
    let mut entries: Vec<DirEntry> = reader
        .filter_map(Result::ok)
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_link = entry.file_type().is_ok_and(|t| t.is_symlink());
            // Follow symlinks so that links to directories sort as directories.
            let meta = fs::metadata(entry.path()).or_else(|_| entry.metadata()).ok();
            let is_dir = meta.as_ref().is_some_and(fs::Metadata::is_dir);
            DirEntry {
                hidden: name.starts_with('.'),
                is_dir,
                size: meta.as_ref().map_or(0, fs::Metadata::len),
                executable: !is_dir && meta.as_ref().is_some_and(is_executable),
                link_target: is_link
                    .then(|| fs::read_link(entry.path()).ok())
                    .flatten()
                    .map(|target| target.display().to_string()),
                name,
            }
        })
        .collect();
    entries.sort_by_cached_key(|e| (e.hidden, !e.is_dir, e.name.to_lowercase(), e.name.clone()));
    Ok(entries)
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Simple layouts

fn directory_lines(entries: &[DirEntry]) -> Vec<Line<'static>> {
    if entries.is_empty() {
        return vec![Line::styled("(empty directory)", NOTE)];
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
                spans.push(Span::styled(format!(" → {}", sanitize(target)), DIM));
            }
            Line::from(spans)
        })
        .collect()
}

fn message_lines(message: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let text: String = message.chars().map(|c| if c == '\n' { c } else { visible_char(c) }).collect();
    wrap_spans(&[Span::styled(text, style)], width).into_iter().map(Line::from).collect()
}

/// Formats a byte count with binary units, e.g. `1.5 KiB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
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

// ---------------------------------------------------------------------------
// Images

/// Scales `image` to fit `max_width`×`max_height` pixels, keeping its aspect ratio.
fn fit_image(image: &RgbaImage, max_width: u32, max_height: u32) -> RgbaImage {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 || max_width == 0 || max_height == 0 {
        return RgbaImage::new(0, 0);
    }
    let scale = (f64::from(max_width) / f64::from(width)).min(f64::from(max_height) / f64::from(height));
    let target_width = ((f64::from(width) * scale).round() as u32).clamp(1, max_width);
    let target_height = ((f64::from(height) * scale).round() as u32).clamp(1, max_height);
    if (target_width, target_height) == (width, height) {
        image.clone()
    } else if target_width >= width && target_height >= height {
        imageops::resize(image, target_width, target_height, FilterType::Nearest)
    } else {
        imageops::thumbnail(image, target_width, target_height)
    }
}

/// Draws `pixels` centred in `area`, two pixel rows per cell with `▀`.
fn draw_half_blocks(pixels: &RgbaImage, area: Rect, buf: &mut Buffer) {
    let (width, height) = pixels.dimensions();
    let columns = width.min(u32::from(area.width));
    let rows = height.div_ceil(2).min(u32::from(area.height));
    // Both offsets are below the area size, which fits in u16.
    let x0 = area.x + ((u32::from(area.width) - columns) / 2) as u16;
    let y0 = area.y + ((u32::from(area.height) - rows) / 2) as u16;
    for (row, y) in (0..rows).zip(y0..) {
        for (column, x) in (0..columns).zip(x0..) {
            let top = opaque(pixels.get_pixel(column, row * 2));
            let bottom = (row * 2 + 1 < height).then(|| opaque(pixels.get_pixel(column, row * 2 + 1))).flatten();
            let cell = &mut buf[(x, y)];
            match (top, bottom) {
                (Some(top), bottom) => {
                    cell.set_char('▀').set_style(Style::new().fg(top).bg(bottom.unwrap_or(Color::Reset)));
                }
                (None, Some(bottom)) => {
                    cell.set_char('▄').set_style(Style::new().fg(bottom).bg(Color::Reset));
                }
                (None, None) => {
                    cell.set_char(' ');
                }
            }
        }
    }
}

fn opaque(pixel: &Rgba<u8>) -> Option<Color> {
    let [r, g, b, a] = pixel.0;
    (a >= 128).then_some(Color::Rgb(r, g, b))
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
    buf.set_stringn(x, y, &label, usize::from(width), INDICATOR);
}

// ---------------------------------------------------------------------------
// Markdown

/// Renders Markdown into lines wrapped to `width` columns.
fn markdown_lines(source: &str, width: usize) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events: Vec<Event<'_>> = Parser::new_ext(source, options).collect();
    MarkdownRenderer::new(width).render(&events)
}

/// A block container (quote or list item) contributing a prefix to each line.
struct Container {
    /// Prefix of the container's first line (e.g. the bullet).
    first: Vec<Span<'static>>,
    /// Prefix of the following lines (the hanging indent).
    rest: Vec<Span<'static>>,
    /// Whether `first` has yet to be emitted.
    fresh: bool,
}

struct ListState {
    /// Next number for ordered lists.
    next: Option<u64>,
    number_width: usize,
}

struct CodeBlock {
    lang: String,
    text: String,
}

#[derive(Default)]
struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    header_rows: usize,
}

struct MarkdownRenderer {
    width: usize,
    lines: Vec<Line<'static>>,
    containers: Vec<Container>,
    lists: Vec<ListState>,
    /// Inline content of the current paragraph, heading or table cell.
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// Open links: destination and where their text starts in `inline`.
    links: Vec<(String, usize)>,
    shown_urls: HashSet<String>,
    code: Option<CodeBlock>,
    html: Option<String>,
    table: Option<TableState>,
    need_blank: bool,
}

impl MarkdownRenderer {
    fn new(width: usize) -> Self {
        MarkdownRenderer {
            width: width.max(1),
            lines: Vec::new(),
            containers: Vec::new(),
            lists: Vec::new(),
            inline: Vec::new(),
            styles: Vec::new(),
            links: Vec::new(),
            shown_urls: HashSet::new(),
            code: None,
            html: None,
            table: None,
            need_blank: false,
        }
    }

    fn render(mut self, events: &[Event<'_>]) -> Vec<Line<'static>> {
        for (index, event) in events.iter().enumerate() {
            self.event(event, &events[index + 1..]);
        }
        self.flush_inline();
        self.lines
    }

    fn event(&mut self, event: &Event<'_>, rest: &[Event<'_>]) {
        match event {
            Event::Start(tag) => self.start(tag, rest),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some(code) = &mut self.code {
                    code.text.push_str(text);
                } else if let Some(html) = &mut self.html {
                    html.push_str(text);
                } else {
                    self.push_text(text, self.style());
                }
            }
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push_text(text, self.style().patch(INLINE_CODE));
            }
            Event::Html(text) => match &mut self.html {
                Some(html) => html.push_str(text),
                None => self.push_inline_html(text),
            },
            Event::InlineHtml(text) => self.push_inline_html(text),
            Event::FootnoteReference(name) => self.push_text(&format!("[^{name}]"), DIM),
            Event::SoftBreak => self.push_text(" ", self.style()),
            Event::HardBreak => self.inline.push(Span::raw("\n")),
            Event::Rule => {
                self.start_block();
                self.emit(vec![Span::styled("─".repeat(self.avail()), RULE)]);
                self.need_blank = true;
            }
            Event::TaskListMarker(done) => self.task_marker(*done),
        }
    }

    fn start(&mut self, tag: &Tag<'_>, rest: &[Event<'_>]) {
        match tag {
            Tag::Paragraph
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => self.start_block(),
            Tag::Heading { level, .. } => {
                self.start_block();
                self.styles.push(heading_style(*level));
            }
            Tag::BlockQuote(_) => {
                self.start_block();
                let bar = vec![Span::styled("│ ", QUOTE_BAR)];
                self.containers.push(Container { first: bar.clone(), rest: bar, fresh: false });
                self.styles.push(QUOTE_TEXT);
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split([' ', '\t', ',', '{']).next().unwrap_or_default().to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeBlock { lang, text: String::new() });
            }
            Tag::HtmlBlock => {
                // The blank-line decision is deferred: comments are dropped.
                self.flush_inline();
                self.html = Some(String::new());
            }
            Tag::List(start) => {
                self.start_block();
                let number_width = start.map_or(0, |first| {
                    let last = first.saturating_add(count_items(rest).saturating_sub(1) as u64);
                    last.to_string().len()
                });
                self.lists.push(ListState { next: *start, number_width });
            }
            Tag::Item => {
                self.start_block();
                let marker = self.next_marker();
                let indent = " ".repeat(marker.width());
                self.containers.push(Container {
                    first: vec![Span::styled(marker, BULLET)],
                    rest: vec![Span::raw(indent)],
                    fresh: true,
                });
            }
            Tag::Table(alignments) => {
                self.start_block();
                self.table = Some(TableState { alignments: alignments.clone(), ..TableState::default() });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.row.clear();
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self.styles.push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.styles.push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Superscript | Tag::Subscript => self.styles.push(Style::new()),
            Tag::Link { dest_url, .. } => {
                self.styles.push(LINK);
                self.links.push((dest_url.to_string(), self.inline.len()));
            }
            Tag::Image { .. } => {
                self.styles.push(IMAGE_ALT);
                self.inline.push(Span::styled("[image: ", self.style()));
            }
        }
    }

    fn end(&mut self, tag: &TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_inline();
                self.need_blank = true;
            }
            TagEnd::Heading(level) => {
                self.styles.pop();
                self.finish_heading(*level);
                self.need_blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.styles.pop();
                self.containers.pop();
                self.need_blank = true;
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.emit_code(&code);
                }
                self.need_blank = true;
            }
            TagEnd::HtmlBlock => {
                if let Some(html) = self.html.take() {
                    self.emit_html(&html);
                }
            }
            TagEnd::List(_) => {
                self.flush_inline();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.need_blank = true;
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                // An empty item still shows its marker.
                if self.containers.last().is_some_and(|c| c.fresh) {
                    self.emit(Vec::new());
                }
                self.containers.pop();
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
                self.need_blank = true;
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    let row = mem::take(&mut table.row);
                    table.rows.push(row);
                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = mem::take(&mut self.inline);
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Superscript | TagEnd::Subscript => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, start)) = self.links.pop() {
                    self.link_suffix(url, start);
                }
            }
            TagEnd::Image => {
                self.inline.push(Span::styled("]", self.style()));
                self.styles.pop();
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {
                self.flush_inline();
                self.need_blank = true;
            }
        }
    }

    /// The current inline style: all open styles patched together.
    fn style(&self) -> Style {
        self.styles.iter().fold(Style::new(), |acc, style| acc.patch(*style))
    }

    fn push_text(&mut self, text: &str, style: Style) {
        let clean: String = text
            .chars()
            .map(|c| match c {
                '\n' | '\r' | '\t' => ' ',
                c => visible_char(c),
            })
            .collect();
        self.inline.push(Span::styled(clean, style));
    }

    fn push_inline_html(&mut self, html: &str) {
        let spans = html_spans(html, self.style());
        self.inline.extend(spans);
    }

    fn task_marker(&mut self, done: bool) {
        let (text, style) = if done { ("[x] ", TASK_DONE) } else { ("[ ] ", TASK_TODO) };
        let ordered = self.lists.last().is_some_and(|list| list.next.is_some());
        match self.containers.last_mut() {
            Some(item) if item.fresh => {
                let marker = Span::styled(text, style);
                if ordered {
                    item.first.push(marker);
                    item.rest.push(Span::raw(" ".repeat(text.len())));
                } else {
                    // Checkboxes replace bullets.
                    item.first = vec![marker];
                    item.rest = vec![Span::raw(" ".repeat(text.len()))];
                }
            }
            _ => self.inline.push(Span::styled(text, style)),
        }
    }

    /// Appends a link's URL after its text, unless redundant or already shown.
    fn link_suffix(&mut self, url: String, start: usize) {
        if url.is_empty() || url.starts_with('#') {
            return;
        }
        let text: String =
            self.inline[start.min(self.inline.len())..].iter().map(|span| span.content.as_ref()).collect();
        let text = text.trim();
        if text == url || Some(text) == url.strip_prefix("mailto:") || !self.shown_urls.insert(url.clone()) {
            return;
        }
        self.push_text(&format!(" ({url})"), LINK_URL);
    }

    fn next_marker(&mut self) -> String {
        let depth = self.lists.len().saturating_sub(1);
        match self.lists.last_mut() {
            Some(ListState { next: Some(number), number_width }) => {
                let marker = format!("{number:>width$}. ", width = *number_width);
                *number = number.saturating_add(1);
                marker
            }
            _ => format!("{} ", BULLETS[depth % BULLETS.len()]),
        }
    }

    /// Width available for content after the container prefixes.
    fn avail(&self) -> usize {
        let prefix: usize = self.containers.iter().map(|c| spans_width(&c.rest)).sum();
        self.width.saturating_sub(prefix).max(1)
    }

    /// Flushes pending inline text and emits a separating blank line if needed.
    fn start_block(&mut self) {
        self.flush_inline();
        if self.need_blank && !self.lines.is_empty() {
            self.emit_blank();
        }
        self.need_blank = false;
    }

    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let spans = mem::take(&mut self.inline);
        for line in wrap_spans(&spans, self.avail()) {
            self.emit(line);
        }
    }

    /// Emits one line of content behind the container prefixes.
    fn emit(&mut self, content: Vec<Span<'static>>) {
        let mut spans = Vec::with_capacity(self.containers.len() + content.len());
        for container in &mut self.containers {
            let prefix = if container.fresh { &container.first } else { &container.rest };
            spans.extend(prefix.iter().cloned());
            container.fresh = false;
        }
        spans.extend(content);
        self.lines.push(Line::from(spans));
    }

    fn emit_blank(&mut self) {
        let mut spans: Vec<Span<'static>> = self.containers.iter().flat_map(|c| c.rest.iter().cloned()).collect();
        while let Some(last) = spans.last_mut() {
            let trimmed = last.content.trim_end();
            if trimmed.is_empty() {
                spans.pop();
            } else {
                if trimmed.len() != last.content.len() {
                    last.content = Cow::Owned(trimmed.to_owned());
                }
                break;
            }
        }
        self.lines.push(Line::from(spans));
    }

    fn finish_heading(&mut self, level: HeadingLevel) {
        let spans = mem::take(&mut self.inline);
        let wrapped = wrap_spans(&spans, self.avail());
        let text_width = wrapped.iter().map(|line| spans_width(line)).max().unwrap_or(0);
        for line in wrapped {
            self.emit(line);
        }
        let style = heading_style(level).remove_modifier(Modifier::BOLD);
        match level {
            HeadingLevel::H1 => self.emit(vec![Span::styled("━".repeat(self.avail()), style)]),
            HeadingLevel::H2 if text_width > 0 => self.emit(vec![Span::styled("─".repeat(text_width), style)]),
            _ => {}
        }
    }

    fn emit_code(&mut self, code: &CodeBlock) {
        let avail = self.avail();
        let indent = if avail >= 8 { 2 } else { 0 };
        let box_width = avail - indent;
        let inner = box_width.saturating_sub(1).max(1);
        let boxed = |text: String, style: Style| {
            let pad = box_width.saturating_sub(1 + text.width());
            vec![Span::raw(" ".repeat(indent)), Span::styled(format!(" {text}{}", " ".repeat(pad)), style)]
        };
        if !code.lang.is_empty() {
            self.emit(boxed(truncate_str(&sanitize(&code.lang), inner), CODE_LABEL));
        }
        let text = code.text.strip_suffix('\n').unwrap_or(&code.text);
        for raw in text.split('\n') {
            let line = expand_tabs(raw.strip_suffix('\r').unwrap_or(raw));
            for chunk in hard_wrap(&line, inner) {
                self.emit(boxed(chunk, CODE_BLOCK));
            }
        }
    }

    /// Shows the text of an HTML block, one line per source line; blocks
    /// made only of tags or comments disappear.
    fn emit_html(&mut self, html: &str) {
        let spans = html_spans(html, self.style());
        let lines: Vec<_> = wrap_spans(&spans, self.avail()).into_iter().filter(|line| spans_width(line) > 0).collect();
        if lines.is_empty() {
            return;
        }
        self.start_block();
        for line in lines {
            self.emit(line);
        }
        self.need_blank = true;
    }

    fn emit_table(&mut self, table: TableState) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0).max(table.alignments.len());
        if columns == 0 {
            return;
        }
        let rows: Vec<Vec<Vec<Span<'static>>>> =
            table.rows.into_iter().map(|row| row.into_iter().map(|cell| single_line(&cell)).collect()).collect();
        let mut widths = vec![1; columns];
        for row in &rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(spans_width(cell));
            }
        }
        // Shrink the widest columns until the table fits.
        let separators = 3 * (columns - 1);
        let mut total = widths.iter().sum::<usize>() + separators;
        let avail = self.avail();
        while total > avail {
            let Some(widest) = widths.iter_mut().max() else {
                break;
            };
            if *widest <= MIN_COLUMN_WIDTH {
                break;
            }
            *widest -= 1;
            total -= 1;
        }
        for (index, row) in rows.iter().enumerate() {
            let header = index < table.header_rows;
            let mut spans = Vec::new();
            for (column, &width) in widths.iter().enumerate() {
                if column > 0 {
                    spans.push(Span::styled(" │ ", TABLE_BORDER));
                }
                let cell = row.get(column).map_or(&[][..], Vec::as_slice);
                let mut cell = truncate_spans(cell, width);
                if header {
                    cell = cell.into_iter().map(|s| s.patch_style(Modifier::BOLD)).collect();
                }
                let gap = width.saturating_sub(spans_width(&cell));
                let (left, right) = match table.alignments.get(column) {
                    Some(Alignment::Right) => (gap, 0),
                    Some(Alignment::Center) => (gap / 2, gap - gap / 2),
                    _ => (0, gap),
                };
                if left > 0 {
                    spans.push(Span::raw(" ".repeat(left)));
                }
                spans.extend(cell);
                if right > 0 {
                    spans.push(Span::raw(" ".repeat(right)));
                }
            }
            self.emit(spans);
            if header && index + 1 == table.header_rows {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                self.emit(vec![Span::styled(rule.join("─┼─"), TABLE_BORDER)]);
            }
        }
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 => bold.fg(Color::LightMagenta),
        HeadingLevel::H2 => bold.fg(Color::LightCyan),
        HeadingLevel::H3 => bold.fg(Color::LightYellow),
        HeadingLevel::H4 => bold.fg(Color::LightGreen),
        HeadingLevel::H5 | HeadingLevel::H6 => bold.add_modifier(Modifier::ITALIC),
    }
}

/// Converts raw HTML to styled text: tags and comments are dropped, `<br>`
/// becomes a line break, `<img>` its alt text, and common entities are decoded.
fn html_spans(html: &str, style: Style) -> Vec<Span<'static>> {
    fn flush(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
        if !text.is_empty() {
            let clean: String = decode_entities(text)
                .chars()
                .map(|c| match c {
                    '\n' => c,
                    '\t' | '\r' => ' ',
                    c => visible_char(c),
                })
                .collect();
            spans.push(Span::styled(clean, style));
            text.clear();
        }
    }
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        let (before, tail) = rest.split_at(start);
        text.push_str(before);
        let end = if tail.starts_with("<!--") {
            Some(tail.find("-->").map_or(tail.len(), |i| i + 3))
        } else if tail[1..].starts_with(|c: char| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?')) {
            tail.find('>').map(|i| i + 1)
        } else {
            None
        };
        let Some(end) = end else {
            // A lone `<` is text.
            text.push('<');
            rest = &tail[1..];
            continue;
        };
        let tag = &tail[..end];
        let name = tag
            .trim_start_matches(['<', '/'])
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match name.as_str() {
            "br" => text.push('\n'),
            "img" => {
                flush(&mut spans, &mut text, style);
                let alt = html_attr(tag, "alt").map(decode_entities).unwrap_or_default();
                let label = if alt.trim().is_empty() {
                    "[image]".to_owned()
                } else {
                    format!("[image: {}]", sanitize(alt.trim()))
                };
                spans.push(Span::styled(label, style.patch(IMAGE_ALT)));
            }
            _ => {}
        }
        rest = &tail[end..];
    }
    text.push_str(rest);
    flush(&mut spans, &mut text, style);
    spans
}

/// The value of attribute `name` in an HTML start tag, if present.
fn html_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    // ASCII lowercasing keeps byte offsets, so indices map back to `tag`.
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let at = from + found;
        from = at + needle.len();
        if !lower[..at].ends_with(char::is_whitespace) {
            continue;
        }
        let value = &tag[from..];
        return match value.chars().next() {
            Some(quote @ ('"' | '\'')) => value[1..].split(quote).next(),
            _ => value.split(|c: char| c.is_whitespace() || c == '>').next(),
        };
    }
    None
}

/// Decodes the handful of HTML entities common in READMEs.
fn decode_entities(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    const ENTITIES: [(&str, &str); 7] = [
        ("&nbsp;", "\u{a0}"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&amp;", "&"),
    ];
    let mut out = text.to_owned();
    for (entity, replacement) in ENTITIES {
        out = out.replace(entity, replacement);
    }
    Cow::Owned(out)
}

/// Counts the items of the list whose events follow its `Start(List)`.
fn count_items(rest: &[Event<'_>]) -> usize {
    let mut depth = 0usize;
    let mut count = 0;
    for event in rest {
        match event {
            Event::Start(Tag::List(_)) => depth += 1,
            Event::End(TagEnd::List(_)) if depth == 0 => break,
            Event::End(TagEnd::List(_)) => depth -= 1,
            Event::Start(Tag::Item) if depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Text helpers

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

/// Expands tabs to 4-column stops and makes control characters visible.
fn expand_tabs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut column = 0;
    for c in line.chars() {
        if c == '\t' {
            let spaces = TAB_WIDTH - column % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            let c = visible_char(c);
            out.push(c);
            column += c.width().unwrap_or(0);
        }
    }
    out
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|span| span.content.as_ref()).collect()
}

/// Cuts `text` to at most `width` columns.
fn truncate_str(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

/// Splits `text` into chunks of at most `width` columns (at least one char each).
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let mut chunks = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && used > 0 {
            chunks.push(String::new());
            used = 0;
        }
        if let Some(chunk) = chunks.last_mut() {
            chunk.push(c);
        }
        used += w;
    }
    chunks
}

/// Joins hard breaks of a table cell into spaces.
fn single_line(spans: &[Span<'static>]) -> Vec<Span<'static>> {
    spans
        .iter()
        .map(|span| {
            if span.content.contains('\n') {
                Span::styled(span.content.replace('\n', " "), span.style)
            } else {
                span.clone()
            }
        })
        .collect()
}

/// Cuts styled text to `width` columns, ending with `…` when shortened.
fn truncate_spans(spans: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    if spans_width(spans) <= width {
        return spans.to_vec();
    }
    if width == 0 {
        return Vec::new();
    }
    let budget = width - 1;
    let mut used = 0;
    let mut out = Vec::new();
    let mut style = Style::new();
    'spans: for span in spans {
        style = span.style;
        let mut text = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > budget {
                if !text.is_empty() {
                    out.push(Span::styled(text, style));
                }
                break 'spans;
            }
            text.push(c);
            used += w;
        }
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
    }
    out.push(Span::styled("…", style));
    out
}

/// Greedy word wrapping of styled text; `\n` forces a break.
fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut wrapper = Wrapper::new(width);
    let mut word: Vec<Span<'static>> = Vec::new();
    for span in spans {
        let mut piece = String::new();
        for c in span.content.chars() {
            if c == ' ' || c == '\n' {
                if !piece.is_empty() {
                    word.push(Span::styled(mem::take(&mut piece), span.style));
                }
                wrapper.word(mem::take(&mut word));
                if c == '\n' {
                    wrapper.break_line();
                } else {
                    wrapper.space(span.style);
                }
            } else {
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            word.push(Span::styled(piece, span.style));
        }
    }
    wrapper.word(word);
    wrapper.finish()
}

struct Wrapper {
    width: usize,
    lines: Vec<Vec<Span<'static>>>,
    line: Vec<Span<'static>>,
    line_width: usize,
    /// A space seen since the last word, emitted only if the next word fits.
    pending_space: Option<Style>,
}

impl Wrapper {
    fn new(width: usize) -> Self {
        Wrapper { width: width.max(1), lines: Vec::new(), line: Vec::new(), line_width: 0, pending_space: None }
    }

    fn space(&mut self, style: Style) {
        if self.line_width > 0 && self.pending_space.is_none() {
            self.pending_space = Some(style);
        }
    }

    fn break_line(&mut self) {
        self.lines.push(mem::take(&mut self.line));
        self.line_width = 0;
        self.pending_space = None;
    }

    fn word(&mut self, word: Vec<Span<'static>>) {
        if word.is_empty() {
            return;
        }
        let width = spans_width(&word);
        let space = usize::from(self.pending_space.is_some());
        if self.line_width + space + width <= self.width {
            if let Some(style) = self.pending_space.take() {
                self.push(" ", style);
            }
            for span in &word {
                self.push(&span.content, span.style);
            }
        } else if width <= self.width {
            self.break_line();
            for span in &word {
                self.push(&span.content, span.style);
            }
        } else {
            // Too long for any line: split it, starting on the current line.
            if let Some(style) = self.pending_space.take() {
                if self.line_width + 1 < self.width {
                    self.push(" ", style);
                } else {
                    self.break_line();
                }
            }
            for span in &word {
                for c in span.content.chars() {
                    if self.line_width + c.width().unwrap_or(0) > self.width && self.line_width > 0 {
                        self.break_line();
                    }
                    self.push(c.encode_utf8(&mut [0; 4]), span.style);
                }
            }
        }
    }

    fn push(&mut self, text: &str, style: Style) {
        self.line_width += text.width();
        match self.line.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(text),
            _ => self.line.push(Span::styled(text.to_owned(), style)),
        }
    }

    fn finish(mut self) -> Vec<Vec<Span<'static>>> {
        if !self.line.is_empty() {
            self.lines.push(self.line);
        }
        self.lines
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A unique scratch directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("kraal-preview-{}-{name}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Rows of `area` as text, skipping the cells hidden by wide characters.
    fn rows(buf: &Buffer, area: Rect) -> Vec<String> {
        (area.top()..area.bottom())
            .map(|y| {
                let mut row = String::new();
                let mut x = area.left();
                while x < area.right() {
                    let symbol = buf[(x, y)].symbol();
                    row.push_str(symbol);
                    x += u16::try_from(symbol.width().max(1)).unwrap();
                }
                row.trim_end().to_owned()
            })
            .collect()
    }

    fn render(preview: &mut Preview, width: u16, height: u16) -> (Buffer, Vec<String>) {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        preview.render(area, &mut buf);
        let text = rows(&buf, area);
        (buf, text)
    }

    fn md(source: &str, width: usize) -> Vec<String> {
        markdown_lines(source, width).iter().map(|l| line_text(l).trim_end().to_owned()).collect()
    }

    fn png(dir: &TempDir, name: &str, width: u32, pixels: &[[u8; 4]]) -> PathBuf {
        let height = u32::try_from(pixels.len()).unwrap() / width;
        let image = RgbaImage::from_fn(width, height, |x, y| Rgba(pixels[(y * width + x) as usize]));
        let path = dir.0.join(name);
        image.save(&path).unwrap();
        path
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    #[test]
    fn markdown_headings_are_styled_and_ruled() {
        let dir = TempDir::new("headings");
        let path = dir.write("README.md", "# Title\n\nHello *world* and **bold**\n\n## Sub\n\n### Third\n");
        let mut preview = Preview::open(&path);
        assert_eq!(preview.kind(), PreviewKind::Markdown);
        assert_eq!(preview.title(), "README.md");
        let (buf, text) = render(&mut preview, 30, 12);
        assert_eq!(text[0], "Title");
        assert_eq!(text[1], "━".repeat(30));
        assert_eq!(text[2], "");
        assert_eq!(text[3], "Hello world and bold");
        assert_eq!(text[5], "Sub");
        assert_eq!(text[6], "───");
        assert_eq!(text[8], "Third");
        let title = &buf[(0, 0)];
        assert!(title.modifier.contains(Modifier::BOLD));
        assert_eq!(title.fg, Color::LightMagenta);
        assert_eq!(buf[(0, 5)].fg, Color::LightCyan);
        assert!(buf[(6, 3)].modifier.contains(Modifier::ITALIC), "world is italic");
        assert!(buf[(16, 3)].modifier.contains(Modifier::BOLD), "bold is bold");
        assert!(!buf[(0, 3)].modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn markdown_inline_code_and_strikethrough() {
        let dir = TempDir::new("inline");
        let path = dir.write("a.md", "use `foo()` not ~~bar~~");
        let mut preview = Preview::open(&path);
        let (buf, text) = render(&mut preview, 40, 3);
        assert_eq!(text[0], "use foo() not bar");
        assert_eq!(buf[(4, 0)].bg, INLINE_CODE.bg.unwrap());
        assert_eq!(buf[(0, 0)].bg, Color::Reset);
        assert!(buf[(14, 0)].modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn markdown_lists_have_hanging_indents() {
        let source = "- one\n- two words that wrap around\n  - nested item\n\n1. first\n2. second\n";
        let lines = md(source, 20);
        assert_eq!(lines[0], "• one");
        assert_eq!(lines[1], "• two words that");
        assert_eq!(lines[2], "  wrap around");
        assert_eq!(lines[3], "  ◦ nested item");
        assert_eq!(lines[4], "");
        assert_eq!(lines[5], "1. first");
        assert_eq!(lines[6], "2. second");
        assert!(lines.iter().all(|l| l.width() <= 20));
    }

    #[test]
    fn markdown_ordered_numbers_are_aligned() {
        let source: String = (1..=10).map(|i| format!("{i}. item {i}\n")).collect();
        let lines = md(&source, 40);
        assert_eq!(lines[0], " 1. item 1");
        assert_eq!(lines[9], "10. item 10");
        let wrapped = md("9. a long item that wraps\n10. b\n", 12);
        assert_eq!(wrapped[0], " 9. a long");
        assert_eq!(wrapped[1], "    item");
    }

    #[test]
    fn markdown_task_lists() {
        let lines = md("- [x] done thing\n- [ ] todo\n", 12);
        assert_eq!(lines, ["[x] done", "    thing", "[ ] todo"]);
    }

    #[test]
    fn markdown_code_blocks_have_label_and_background() {
        let dir = TempDir::new("code");
        let path = dir.write("c.md", "Text\n\n```rust\nfn main() {\n\tx();\n}\n```\n");
        let mut preview = Preview::open(&path);
        let (buf, text) = render(&mut preview, 24, 8);
        assert_eq!(text[0], "Text");
        assert_eq!(text[2], "   rust");
        assert_eq!(text[3], "   fn main() {");
        assert_eq!(text[4], "       x();");
        assert_eq!(text[5], "   }");
        assert_eq!(buf[(0, 3)].bg, Color::Reset, "indent has no background");
        assert_eq!(buf[(2, 3)].bg, CODE_BLOCK.bg.unwrap());
        assert_eq!(buf[(23, 3)].bg, CODE_BLOCK.bg.unwrap(), "background spans the width");
        assert!(buf[(3, 2)].modifier.contains(Modifier::ITALIC), "label is italic");
    }

    #[test]
    fn markdown_long_code_lines_wrap_inside_the_box() {
        let lines = md("```\nabcdefghijklmnop\n```", 12);
        assert_eq!(lines, ["   abcdefghi", "   jklmnop"]);
    }

    #[test]
    fn markdown_block_quotes() {
        let lines = md("> quoted text that wraps\n>\n> second\n\nafter", 16);
        assert_eq!(lines, ["│ quoted text", "│ that wraps", "│", "│ second", "", "after"]);
        let nested = md("> outer\n> > inner", 20);
        assert_eq!(nested, ["│ outer", "│", "│ │ inner"]);
    }

    #[test]
    fn markdown_tables_align_and_truncate() {
        let source = "| Name | Qty |\n|:-----|----:|\n| apple | 3 |\n| kiwi | 12 |\n";
        let lines = md(source, 40);
        assert_eq!(lines, ["Name  │ Qty", "──────┼────", "apple │   3", "kiwi  │  12"]);
        let narrow = md(source, 10);
        assert!(narrow.iter().all(|l| l.width() <= 10), "{narrow:?}");
        assert_eq!(narrow[2], "app… │   3");
        let header = markdown_lines(source, 40);
        assert!(header[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn markdown_links_show_url_once() {
        let lines = md("[docs](https://x.io) and [docs](https://x.io) and <https://y.io>", 80);
        assert_eq!(lines, ["docs (https://x.io) and docs and https://y.io"]);
        let rendered = markdown_lines("[docs](https://x.io)", 80);
        let link = &rendered[0].spans[0];
        assert_eq!(link.content, "docs");
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(rendered[0].spans[1].style, LINK_URL);
    }

    #[test]
    fn markdown_images_rules_and_breaks() {
        assert_eq!(md("![a cat](cat.png)", 40), ["[image: a cat]"]);
        assert_eq!(md("above\n\n---\n\nbelow", 5), ["above", "", "─────", "", "below"]);
        assert_eq!(md("one  \ntwo<br>three", 40), ["one", "two", "three"]);
        assert_eq!(md("<!-- hidden -->\n\ntext", 40), ["text"]);
    }

    #[test]
    fn markdown_html_is_reduced_to_text() {
        let block = "<p align=\"center\">\n  <img src=\"logo.png\" alt=\"Logo &amp; name\">\n</p>\n\ntext";
        assert_eq!(md(block, 40), ["[image: Logo & name]", "", "text"]);
        let details = "<details>\n<summary>More &lt;info&gt;</summary>\n\nbody\n\n</details>\n";
        assert_eq!(md(details, 40), ["More <info>", "", "body"]);
        assert_eq!(md("Press <kbd>Ctrl</kbd> + <img src=x.png> a < b", 40), ["Press Ctrl + [image] a < b"]);
        assert_eq!(html_attr("<img ALT='single' src=x>", "alt"), Some("single"));
        assert_eq!(html_attr("<img data-alt=\"no\" alt=bare>", "alt"), Some("bare"));
        assert_eq!(html_attr("<img src=x>", "alt"), None);
    }

    #[test]
    fn markdown_rewraps_per_width() {
        let dir = TempDir::new("wrap");
        let words = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor";
        let path = dir.write("w.md", words);
        let mut preview = Preview::open(&path);
        let (_, wide) = render(&mut preview, 80, 10);
        assert_eq!(wide[0], words);
        let (_, narrow) = render(&mut preview, 20, 10);
        assert!(narrow.iter().all(|l| l.width() <= 20));
        assert_eq!(narrow[0], "lorem ipsum dolor");
        assert_eq!(narrow.join(" ").split_whitespace().collect::<Vec<_>>().join(" "), words);
    }

    #[test]
    fn markdown_respects_display_width() {
        let lines = md("中文字符测试 emoji 🎉🎉 more 漢字漢字漢字漢字", 10);
        assert!(lines.iter().all(|l| l.width() <= 10), "{lines:?}");
        assert!(lines.concat().contains("🎉🎉"));
        // A width narrower than a wide character still terminates.
        assert!(!md("漢字", 1).is_empty());
    }

    #[test]
    fn markdown_survives_odd_input_at_tiny_widths() {
        let source = "# H\n> - [x] a\n>   1. `code`\n\n| a | b |\n|---|---|\n| 中 | x |\n\n```\n\t\x07\n```\n- \n";
        for width in 1..12 {
            for line in markdown_lines(source, width) {
                let _ = line_text(&line);
            }
        }
    }

    #[test]
    fn image_renders_half_blocks() {
        let dir = TempDir::new("image");
        let path = png(&dir, "tiny.png", 2, &[RED, GREEN, BLUE, WHITE]);
        let mut preview = Preview::open(&path);
        assert_eq!(preview.kind(), PreviewKind::Image);
        assert_eq!(preview.visible_text(), "[image 2×2 png]");
        let (buf, text) = render(&mut preview, 2, 1);
        assert_eq!(text, ["▀▀"]);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(255, 0, 0));
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(0, 0, 255));
        assert_eq!(buf[(1, 0)].fg, Color::Rgb(0, 255, 0));
        assert_eq!(buf[(1, 0)].bg, Color::Rgb(255, 255, 255));
    }

    #[test]
    fn image_is_scaled_and_centered() {
        let dir = TempDir::new("center");
        let path = png(&dir, "tiny.png", 2, &[RED, GREEN, BLUE, WHITE]);
        let mut preview = Preview::open(&path);
        // 10×2 cells = 10×4 pixels: the square image becomes 4×4, centered.
        let (buf, text) = render(&mut preview, 10, 2);
        assert_eq!(text, ["   ▀▀▀▀", "   ▀▀▀▀"]);
        assert_eq!(buf[(3, 0)].fg, Color::Rgb(255, 0, 0));
        assert_eq!(buf[(3, 0)].bg, Color::Rgb(255, 0, 0));
        assert_eq!(buf[(5, 0)].fg, Color::Rgb(0, 255, 0));
        assert_eq!(buf[(3, 1)].fg, Color::Rgb(0, 0, 255));
        assert_eq!(buf[(6, 1)].bg, Color::Rgb(255, 255, 255));
        // A wide image in a tall area is centered vertically.
        let wide = png(&dir, "wide.png", 4, &[RED, RED, RED, RED]);
        let mut preview = Preview::open(&wide);
        let (_, text) = render(&mut preview, 4, 5);
        assert_eq!(text, ["", "", "▀▀▀▀", "", ""]);
    }

    #[test]
    fn image_transparency_and_odd_heights() {
        let clear = [0, 0, 0, 0];
        let dir = TempDir::new("alpha");
        let path = png(&dir, "a.png", 2, &[clear, RED, GREEN, clear, BLUE, BLUE]);
        let mut preview = Preview::open(&path);
        let (buf, text) = render(&mut preview, 2, 2);
        assert_eq!(text, ["▄▀", "▀▀"]);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(0, 255, 0));
        assert_eq!(buf[(0, 0)].bg, Color::Reset);
        assert_eq!(buf[(0, 1)].bg, Color::Reset, "no pixel row below the last one");
    }

    #[test]
    fn image_sniffed_without_extension_and_decode_errors() {
        let dir = TempDir::new("sniff");
        let real = png(&dir, "real.png", 1, &[RED]);
        let disguised = dir.write("blob.dat", fs::read(real).unwrap());
        assert_eq!(Preview::open(&disguised).kind(), PreviewKind::Image);
        let broken = dir.write("broken.png", b"not an image at all");
        let mut preview = Preview::open(&broken);
        assert_eq!(preview.kind(), PreviewKind::Image);
        let (buf, text) = render(&mut preview, 40, 4);
        assert!(text.join(" ").contains("broken.png"), "{text:?}");
        assert_eq!(buf[(0, 0)].fg, Color::Red);
    }

    #[test]
    fn directory_listing_order_and_styles() {
        let dir = TempDir::new("dir");
        fs::create_dir(dir.0.join("src")).unwrap();
        fs::create_dir(dir.0.join("Assets")).unwrap();
        fs::create_dir(dir.0.join(".git")).unwrap();
        dir.write("b.txt", vec![b'x'; 1536]);
        dir.write("a.txt", "hi");
        dir.write(".env", "");
        let mut preview = Preview::open(&dir.0);
        assert_eq!(preview.kind(), PreviewKind::Directory);
        let (buf, text) = render(&mut preview, 30, 8);
        let expected = [
            "           Assets/",
            "           src/",
            "      2 B  a.txt",
            "  1.5 KiB  b.txt",
            "           .git/",
            "      0 B  .env",
        ];
        assert_eq!(text[..6], expected);
        assert_eq!(buf[(11, 0)].fg, DIR_NAME.fg.unwrap());
        assert!(buf[(11, 4)].modifier.contains(Modifier::DIM), "hidden entries are dimmed");
        assert!(!buf[(11, 0)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(15 * 1024 * 1024), "15 MiB");
        assert_eq!(human_size(u64::MAX), "16384 PiB");
    }

    #[test]
    fn text_has_gutter_and_expanded_tabs() {
        let dir = TempDir::new("text");
        let path = dir.write("main.rs", "fn main() {\n\tprintln!(\"hi\");\r\n}\n");
        let mut preview = Preview::open(&path);
        assert_eq!(preview.kind(), PreviewKind::Text);
        let (buf, text) = render(&mut preview, 40, 5);
        assert_eq!(text[0], "  1 │ fn main() {");
        assert_eq!(text[1], "  2 │     println!(\"hi\");");
        assert_eq!(text[2], "  3 │ }");
        assert_eq!(text[3], "");
        assert_eq!(buf[(2, 0)].fg, GUTTER.fg.unwrap());
        assert_eq!(buf[(6, 0)].fg, Color::Reset);
        // No wrapping: long lines are cut at the edge.
        let (_, narrow) = render(&mut preview, 12, 3);
        assert_eq!(narrow[1], "  2 │     pr");
    }

    #[test]
    fn text_shows_control_characters_and_empty_files() {
        let dir = TempDir::new("ctrl");
        let mut preview = Preview::open(&dir.write("c.txt", "a\x1bb"));
        let (_, text) = render(&mut preview, 20, 2);
        assert_eq!(text[0], "  1 │ a\u{241b}b");
        let mut empty = Preview::open(&dir.write("empty", ""));
        assert_eq!(empty.kind(), PreviewKind::Text);
        let (_, text) = render(&mut empty, 20, 2);
        assert_eq!(text[0], "(empty file)");
    }

    #[test]
    fn binary_detection() {
        let dir = TempDir::new("binary");
        let mut nul = Preview::open(&dir.write("data", b"abc\0def"));
        assert_eq!(nul.kind(), PreviewKind::Binary);
        let (_, text) = render(&mut nul, 40, 2);
        assert_eq!(text[0], "binary file, 7 bytes");
        let latin1 = Preview::open(&dir.write("latin1.txt", b"caf\xe9"));
        assert_eq!(latin1.kind(), PreviewKind::Binary);
        let big = Preview::open(&dir.write("big", vec![0u8; 2048]));
        assert_eq!(big.visible_text(), "binary file, 2048 bytes (2.0 KiB)");
    }

    #[test]
    fn scrolling_is_clamped() {
        let dir = TempDir::new("scroll");
        let body: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let mut preview = Preview::open(&dir.write("long.txt", body));
        let (_, text) = render(&mut preview, 30, 10);
        assert_eq!(text[0], "  1 │ line 1");
        assert!(text[9].ends_with(" 0%"), "{:?}", text[9]);
        preview.scroll(-5);
        assert_eq!(preview.visible_text().lines().next(), Some("  1 │ line 1"));
        preview.scroll(1000);
        assert_eq!(preview.visible_text().lines().next(), Some(" 91 │ line 91"));
        preview.top();
        preview.page_down();
        assert_eq!(preview.visible_text().lines().next(), Some(" 11 │ line 11"));
        preview.page_up();
        preview.page_up();
        assert_eq!(preview.visible_text().lines().next(), Some("  1 │ line 1"));
        preview.bottom();
        let (buf, text) = render(&mut preview, 30, 10);
        assert_eq!(text[0], " 91 │ line 91");
        assert_eq!(text[9], "100 │ line 100           100%");
        assert_eq!(buf[(29, 9)].fg, INDICATOR.fg.unwrap());
        // Growing the area clamps the offset again and hides the indicator.
        let (_, text) = render(&mut preview, 30, 200);
        assert_eq!(text[0], "  1 │ line 1");
        assert!(!text.iter().any(|l| l.contains('%')));
    }

    #[test]
    fn scrolling_before_render_and_on_images() {
        let dir = TempDir::new("prerender");
        let body: String = (1..=50).map(|i| format!("{i}\n")).collect();
        let mut preview = Preview::open(&dir.write("n.txt", body));
        preview.bottom();
        preview.scroll(3);
        let (_, text) = render(&mut preview, 20, 10);
        assert_eq!(text[0], " 41 │ 41");
        let mut image = Preview::open(&png(&dir, "i.png", 1, &[RED]));
        image.scroll(10);
        image.bottom();
        image.page_down();
        let (_, text) = render(&mut image, 4, 2);
        assert_eq!(text, ["▀▀▀▀", "▀▀▀▀"]);
    }

    #[test]
    fn zero_sized_and_offscreen_areas_do_not_panic() {
        let dir = TempDir::new("zero");
        let paths = [
            dir.write("a.md", "# x\n\n| a |\n|---|\n| b |\n"),
            dir.write("b.txt", "text"),
            dir.write("c", b"\0\0"),
            png(&dir, "d.png", 1, &[RED]),
            dir.0.clone(),
            dir.0.join("missing"),
        ];
        let mut buf = Buffer::empty(Rect::new(0, 0, 5, 5));
        for path in &paths {
            let mut preview = Preview::open(path);
            for area in [
                Rect::new(0, 0, 0, 0),
                Rect::new(0, 0, 0, 5),
                Rect::new(0, 0, 5, 0),
                Rect::new(1, 1, 1, 1),
                Rect::new(4, 4, 10, 10),
                Rect::new(50, 50, 10, 10),
            ] {
                preview.render(area, &mut buf);
                preview.scroll(-3);
                preview.page_down();
                preview.bottom();
                let _ = preview.visible_text();
            }
        }
    }

    #[test]
    fn render_stays_inside_area() {
        let dir = TempDir::new("inside");
        let long: String = (0..50).map(|i| format!("a very long line number {i} 漢字漢字\n")).collect();
        let paths = [
            dir.write("a.md", format!("# Title\n\n{long}\n\n```\n{long}```\n")),
            dir.write("b.txt", &long),
            png(&dir, "c.png", 2, &[RED, GREEN, BLUE, WHITE]),
            dir.0.clone(),
        ];
        let outer = Rect::new(0, 0, 30, 12);
        let area = Rect::new(5, 3, 17, 6);
        for path in &paths {
            let mut buf = Buffer::filled(outer, ratatui::buffer::Cell::new("X"));
            let mut preview = Preview::open(path);
            preview.bottom();
            preview.render(area, &mut buf);
            for y in outer.top()..outer.bottom() {
                for x in outer.left()..outer.right() {
                    if !area.contains((x, y).into()) {
                        assert_eq!(buf[(x, y)].symbol(), "X", "{path:?} wrote at {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn unreadable_path_renders_red_error() {
        let mut preview = Preview::open(Path::new("/definitely/not/here.txt"));
        assert_eq!(preview.kind(), PreviewKind::Text);
        assert_eq!(preview.title(), "here.txt");
        let (buf, text) = render(&mut preview, 30, 5);
        assert!(text[0].starts_with("cannot read"), "{text:?}");
        assert!(text.iter().all(|l| l.width() <= 30));
        assert_eq!(buf[(0, 0)].fg, Color::Red);
        assert!(!preview.modified_since_load());
    }

    #[test]
    fn reload_follows_file_changes() {
        let dir = TempDir::new("reload");
        let path = dir.write("notes.txt", "first\n");
        let mut preview = Preview::open(&path);
        assert!(!preview.modified_since_load());
        fs::write(&path, "second version\n").unwrap();
        assert!(preview.modified_since_load());
        preview.reload();
        assert!(!preview.modified_since_load());
        assert_eq!(preview.visible_text(), "  1 │ second version");
        fs::remove_file(&path).unwrap();
        assert!(preview.modified_since_load());
        preview.reload();
        assert!(preview.visible_text().starts_with("cannot read"));
    }

    #[test]
    fn titles() {
        assert_eq!(Preview::open(Path::new("/tmp/x/README.md")).title(), "README.md");
        assert_eq!(Preview::open(Path::new("/")).title(), "/");
        let dir = TempDir::new("title");
        let name = dir.0.file_name().unwrap().to_string_lossy().into_owned();
        fs::create_dir(dir.0.join("sub")).unwrap();
        assert_eq!(Preview::open(&dir.0.join("sub/..")).title(), name);
    }
}
