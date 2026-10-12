//! Images in panes: the kitty graphics protocol and sixel, shown in the outer terminal.
//!
//! A pane's images never reach the outer terminal as the program sent them. kraal keeps each
//! one (as PNG), gives it an id of its own, and writes Unicode placeholder cells (U+10EEEE, as
//! kitty's protocol defines them) into the pane's grid where the image goes: the colour of a
//! cell names the image, combining characters its row and column. The cells scroll, clip and
//! hide under other panes like any text. Each client whose terminal speaks the protocol gets
//! the images it needs (see `server::graphics`), and the cells render as the image there. A
//! terminal that draws sixel instead gets each visible part of an image as a sixel image drawn
//! over the cells (see [`paint`]); for the others, the cells render blank.

mod diacritics;
pub mod paint;
pub mod quantize;
mod scan;
pub mod sixel;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AColor, Handler, Rgb};
use base64::Engine;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

pub use diacritics::DIACRITICS;
pub use scan::Piece;

/// The character of a placeholder cell.
pub const PLACEHOLDER: char = '\u{10EEEE}';
/// The high byte of every id kraal gives an image: it tells kraal's cells from those a
/// program writes with ids of its own.
const KRAAL_ID: u32 = 0x4b00_0000;
/// Largest image kept, per side and in pixels.
const MAX_SIDE: u32 = 10_000;
const MAX_PIXELS: u64 = 40_000_000;
/// Largest transmission assembled from chunks (base64).
const MAX_UPLOAD: usize = 64 << 20;
/// Largest PNG kept: it must fit, encoded, in one frame to a client.
const MAX_PNG: usize = 24 << 20;
/// Largest image a placement spans, in cells: as far as the diacritics count.
const MAX_CELLS: usize = DIACRITICS.len();
/// Cell size assumed until the outer terminal says.
const DEFAULT_CELL: (u16, u16) = (10, 20);
/// Images a pane shows at once, and placements of one image: beyond, the oldest go.
const MAX_SHOWN: usize = 1024;
const MAX_PLACEMENTS: usize = 64;
/// Bytes counted for each picture besides its PNG, so that many tiny images fill the quota too.
const PICTURE_COST: usize = 1024;
/// Placed images a pane holds before kraal looks for those whose cells are all gone.
const SWEEP_START: usize = 32;
/// Pixels of a pane's images decoded per second: a few bytes of output describe a picture of 40
/// million pixels (a sixel size, a crop of a large image), and the work each takes is in
/// proportion to its pixels. Past it, the pane's images are refused (kitty) or dropped (sixel)
/// until the next second; one image, however large, always fits in a second that spent nothing.
const PIXELS_PER_SECOND: u64 = 100_000_000;

const LENIENT: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new()
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
);

/// What the server and every pane share about images.
pub struct Shared {
    enabled: bool,
    /// An attached client can show images: programs are told they can send them.
    available: AtomicBool,
    /// The outer terminal's cell size in pixels (`width << 16 | height`), 0 until known.
    cell: AtomicU32,
    /// Bytes of image data held by all panes, and the most they may hold.
    used: AtomicUsize,
    quota: usize,
    next_id: AtomicU32,
    /// Changes whenever an image is shown or goes: clients compare it to what they have.
    version: AtomicU64,
}

impl Default for Shared {
    fn default() -> Self {
        Shared::new(true, 256 << 20)
    }
}

impl Shared {
    pub fn new(enabled: bool, quota: usize) -> Shared {
        Shared {
            enabled,
            available: AtomicBool::new(false),
            cell: AtomicU32::new(0),
            used: AtomicUsize::new(0),
            quota,
            next_id: AtomicU32::new(1),
            version: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_available(&self, available: bool) {
        self.available.store(available && self.enabled, Ordering::Relaxed);
    }

    pub fn available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    pub fn set_cell(&self, width: u16, height: u16) {
        if width > 0 && height > 0 {
            self.cell.store(u32::from(width) << 16 | u32::from(height), Ordering::Relaxed);
        }
    }

    /// The outer terminal's cell size in pixels, or a common one until it is known.
    pub fn cell(&self) -> (u16, u16) {
        match self.cell.load(Ordering::Relaxed) {
            0 => DEFAULT_CELL,
            v => ((v >> 16) as u16, v as u16),
        }
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// The most image data all panes may hold, in bytes.
    pub fn quota(&self) -> usize {
        self.quota
    }

    /// An image was shown or went: the clients' terminals are brought up to date.
    pub fn changed(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// A new id for an image kraal shows.
    pub fn new_id(&self) -> u32 {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed) & 0x00ff_ffff;
        KRAAL_ID | n.max(1)
    }
}

/// An image as kraal keeps it: PNG, whatever the program sent.
pub struct Picture {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    shared: Arc<Shared>,
}

impl Picture {
    /// A PNG of `width × height` pixels, counted in the quota while it lives: the pane that keeps
    /// it makes room for it (see [`PaneGraphics::make_room`]).
    fn new(png: Vec<u8>, width: u32, height: u32, shared: &Arc<Shared>) -> Arc<Picture> {
        shared.used.fetch_add(png.len() + PICTURE_COST, Ordering::Relaxed);
        Arc::new(Picture { png, width, height, shared: shared.clone() })
    }

    /// [`Picture::new`] for a picture that cannot make room for itself (a preview's: the panes'
    /// images are not its to drop): only when it fits in what the quota has left.
    pub fn within_quota(png: Vec<u8>, width: u32, height: u32, shared: &Arc<Shared>) -> Option<Arc<Picture>> {
        let cost = png.len() + PICTURE_COST;
        let mut used = shared.used.load(Ordering::Relaxed);
        loop {
            let total = used.checked_add(cost).filter(|total| *total <= shared.quota)?;
            match shared.used.compare_exchange_weak(used, total, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(now) => used = now,
            }
        }
        Some(Arc::new(Picture { png, width, height, shared: shared.clone() }))
    }

    fn checked(png: Vec<u8>, width: u32, height: u32, shared: &Arc<Shared>) -> Result<Arc<Picture>, String> {
        if png.len() > MAX_PNG {
            return Err("EFBIG:image too large".into());
        }
        Ok(Picture::new(png, width, height, shared))
    }

    fn from_rgba(width: u32, height: u32, rgba: Vec<u8>, shared: &Arc<Shared>) -> Result<Arc<Picture>, String> {
        Picture::checked(encode_png(width, height, &rgba)?, width, height, shared)
    }

    pub fn decode(&self) -> Result<image::DynamicImage, String> {
        decode_png(&self.png)
    }
}

/// `width × height` RGBA pixels as PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    if rgba.len() != width as usize * height as usize * 4 {
        return Err("EINVAL:size does not match the data".into());
    }
    // Fast rather than small: the PNG only travels to the terminal next to it.
    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    );
    encoder.write_image(rgba, width, height, image::ExtendedColorType::Rgba8).map_err(|e| format!("EINVAL:{e}"))?;
    Ok(png)
}

impl Drop for Picture {
    fn drop(&mut self) {
        self.shared.used.fetch_sub(self.png.len() + PICTURE_COST, Ordering::Relaxed);
    }
}

fn decode_png(png: &[u8]) -> Result<image::DynamicImage, String> {
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_PIXELS * 4);
    reader.limits(limits);
    reader.decode().map_err(|e| format!("EBADPNG:{e}"))
}

/// An image on show in a pane, under the id kraal gave it.
pub struct Shown {
    pub picture: Arc<Picture>,
    /// Its placements in the outer terminal: placement id, columns, rows. One for an image
    /// kraal placed; a program's own placeholders may have several.
    pub placements: Vec<(u32, u16, u16)>,
    /// Changes with the placements.
    pub generation: u64,
    /// The program's id for it (0 when it gave none).
    origin: u32,
    /// Placed by kraal at the cursor, as opposed to a program's own placeholders.
    direct: bool,
    /// Placed while the alternate screen was up: its cells went with it.
    alt: bool,
    stamp: u64,
}

struct Upload {
    picture: Arc<Picture>,
    stamp: u64,
}

/// The pixels a pane's images took to decode in the current second (see [`PIXELS_PER_SECOND`]).
struct Budget {
    since: std::time::Instant,
    spent: u64,
    limit: u64,
}

impl Budget {
    fn new() -> Budget {
        Budget { since: std::time::Instant::now(), spent: 0, limit: PIXELS_PER_SECOND }
    }

    fn renew(&mut self) {
        if self.since.elapsed() >= std::time::Duration::from_secs(1) {
            self.since = std::time::Instant::now();
            self.spent = 0;
        }
    }

    /// Whether nothing more may be decoded this second.
    fn spent(&mut self) -> bool {
        self.renew();
        self.spent >= self.limit
    }

    /// Count `pixels` decoded; whether they fit, when asked before.
    fn spend(&mut self, pixels: u64) -> bool {
        self.renew();
        if self.spent > 0 && self.spent.saturating_add(pixels) > self.limit {
            return false;
        }
        self.spent = self.spent.saturating_add(pixels);
        true
    }
}

const BUSY: &str = "EBUSY:too many images at once";

/// A transmission arriving in chunks.
struct Pending {
    command: Command,
    payload: Vec<u8>,
}

/// A pane's images: what its program sent, and what is on show.
pub struct PaneGraphics {
    shared: Arc<Shared>,
    scanner: scan::Scanner,
    pending: Option<Pending>,
    /// Images by the program's id.
    uploads: HashMap<u32, Upload>,
    /// The newest image with a number (`I`), by number.
    numbers: HashMap<u32, u32>,
    shown: HashMap<u32, Shown>,
    /// The program's id → kraal's, for images the program shows with its own placeholders.
    virtual_ids: HashMap<u32, u32>,
    /// Placements the program named (image id, placement id) → kraal's id.
    named: HashMap<(u32, u32), u32>,
    clock: u64,
    /// How many placed images trigger the next look for those whose cells are gone.
    sweep_at: usize,
    /// Images with cells in the part of the history kept outside the emulator.
    pub archived: HashSet<u32>,
    budget: Budget,
}

impl Drop for PaneGraphics {
    fn drop(&mut self) {
        // What this pane showed goes from the clients' terminals with the next frame.
        if !self.shown.is_empty() {
            self.shared.changed();
        }
    }
}

impl PaneGraphics {
    pub fn new(shared: Arc<Shared>) -> PaneGraphics {
        PaneGraphics {
            shared,
            scanner: scan::Scanner::default(),
            pending: None,
            uploads: HashMap::new(),
            numbers: HashMap::new(),
            shown: HashMap::new(),
            virtual_ids: HashMap::new(),
            named: HashMap::new(),
            clock: 0,
            sweep_at: SWEEP_START,
            archived: HashSet::new(),
            budget: Budget::new(),
        }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Output for the emulator, with images taken out. Without image support configured, all
    /// of it goes to the emulator, which ignores these sequences.
    pub fn split<'a>(&mut self, bytes: &'a [u8]) -> Vec<Piece<'a>> {
        if !self.shared.enabled {
            return vec![Piece::Text(bytes.into())];
        }
        self.scanner.split(bytes)
    }

    /// Images on show, by kraal's id.
    pub fn shown(&self) -> impl Iterator<Item = (u32, &Shown)> {
        self.shown.iter().map(|(k, s)| (*k, s))
    }

    /// Act on an image sequence; returns the reply for the program, if any.
    pub fn handle<T: EventListener>(&mut self, piece: Piece<'_>, term: &mut Term<T>) -> Option<Vec<u8>> {
        match piece {
            Piece::Text(_) => None,
            Piece::Kitty(body) => self.kitty(&body, term),
            Piece::Sixel { data, .. } => {
                self.sixel(&data, term);
                None
            }
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    // ---- kitty graphics commands -----------------------------------------------------

    fn kitty<T: EventListener>(&mut self, body: &[u8], term: &mut Term<T>) -> Option<Vec<u8>> {
        let mut cmd = match Command::parse(body) {
            Ok(c) => c,
            Err(e) => return Command::default().reply(0, e),
        };
        // The chunks of a transmission carry only `m` (and `q`); a deletion ends it.
        if cmd.action != Some(b'd')
            && let Some(mut pending) = self.pending.take()
        {
            if pending.payload.len() + cmd.payload.len() > MAX_UPLOAD {
                return pending.command.reply(pending.command.id, "EFBIG:image too large".into());
            }
            pending.payload.extend_from_slice(&cmd.payload);
            if cmd.more {
                self.pending = Some(pending);
                return None;
            }
            let mut command = pending.command;
            command.payload = pending.payload;
            return self.transmit(command, term);
        }
        match cmd.action.unwrap_or(b't') {
            b't' | b'T' | b'q' if cmd.more => {
                self.pending = Some(Pending { payload: std::mem::take(&mut cmd.payload), command: cmd });
                None
            }
            b't' | b'T' | b'q' => self.transmit(cmd, term),
            b'p' => {
                if !self.shared.available() {
                    return None;
                }
                let id = match self.resolve_app(&cmd) {
                    Ok(id) => id,
                    Err(e) => return cmd.reply(cmd.id, e),
                };
                let Some(picture) = self.uploads.get_mut(&id).map(|u| {
                    u.stamp = self.clock + 1;
                    u.picture.clone()
                }) else {
                    return cmd.reply(id, "ENOENT:no such image".into());
                };
                self.tick();
                match self.place(&cmd, id, picture, term) {
                    Ok(()) => cmd.reply(id, "OK".into()),
                    Err(e) => cmd.reply(id, e),
                }
            }
            b'd' => {
                self.delete(&cmd, term);
                None
            }
            _ => cmd.reply(cmd.id, "EINVAL:animation is not supported".into()),
        }
    }

    /// The program's image id a command names, through its number when it gives one.
    fn resolve_app(&self, cmd: &Command) -> Result<u32, String> {
        match (cmd.id, cmd.number) {
            (0, 0) => Err("EINVAL:no image id or number".into()),
            (id, 0) => Ok(id),
            (0, n) => self.numbers.get(&n).copied().ok_or_else(|| "ENOENT:no image with this number".into()),
            _ => Err("EINVAL:both an id and a number".into()),
        }
    }

    fn transmit<T: EventListener>(&mut self, mut cmd: Command, term: &mut Term<T>) -> Option<Vec<u8>> {
        // Like a terminal without images: nothing kept, nothing answered.
        if !self.shared.available() {
            return None;
        }
        if cmd.id != 0 && cmd.number != 0 {
            return cmd.reply(cmd.id, "EINVAL:both an id and a number".into());
        }
        let query = cmd.action == Some(b'q');
        let picture = match self.load(&mut cmd) {
            Ok(p) => p,
            Err(e) => return cmd.reply(cmd.id, e),
        };
        if query {
            return cmd.reply(cmd.id, "OK".into());
        }
        if let Err(e) = self.make_room() {
            return cmd.reply(cmd.id, e);
        }
        // An id or a number keeps the image for later placements; sending an id again
        // replaces that image, and what showed it goes.
        let id = match (cmd.id, cmd.number) {
            (0, 0) => 0,
            (0, n) => {
                let id = self.shared.new_id();
                self.numbers.insert(n, id);
                id
            }
            (id, _) => {
                self.forget_origin(id, None, true);
                id
            }
        };
        let stamp = self.tick();
        if id != 0 {
            self.uploads.insert(id, Upload { picture: picture.clone(), stamp });
        }
        if cmd.action == Some(b'T')
            && let Err(e) = self.place(&cmd, id, picture, term)
        {
            return cmd.reply(id, e);
        }
        cmd.reply(id, "OK".into())
    }

    /// The picture a transmission carries.
    fn load(&mut self, cmd: &mut Command) -> Result<Arc<Picture>, String> {
        if cmd.medium != b'd' {
            // Files and shared memory would be this machine's, not the program's when it runs
            // elsewhere (over ssh): programs fall back to sending the data.
            return Err("EBADF:Failed to read image file".into());
        }
        let mut data = LENIENT.decode(&cmd.payload).map_err(|_| "EINVAL:bad base64 data".to_string())?;
        cmd.payload = Vec::new();
        if cmd.compressed {
            let limit = (MAX_PIXELS * 4) as usize;
            data = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&data, limit)
                .map_err(|_| "EINVAL:bad compressed data".to_string())?;
        }
        match cmd.format {
            100 => {
                if self.budget.spent() {
                    return Err(BUSY.into());
                }
                let img = decode_png(&data)?;
                self.budget.spend(u64::from(img.width()) * u64::from(img.height()));
                Picture::checked(data, img.width(), img.height(), &self.shared)
            }
            24 | 32 => {
                let (w, h) = (cmd.width, cmd.height);
                if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || u64::from(w) * u64::from(h) > MAX_PIXELS {
                    return Err("EINVAL:bad image size".into());
                }
                if !self.budget.spend(u64::from(w) * u64::from(h)) {
                    return Err(BUSY.into());
                }
                let bpp = if cmd.format == 24 { 3 } else { 4 };
                if data.len() != w as usize * h as usize * bpp {
                    return Err("ENODATA:size does not match the data".into());
                }
                let rgba = if bpp == 4 {
                    data
                } else {
                    data.as_chunks::<3>().0.iter().flat_map(|[r, g, b]| [*r, *g, *b, 255]).collect()
                };
                Picture::from_rgba(w, h, rgba, &self.shared)
            }
            _ => Err("EINVAL:unknown format".into()),
        }
    }

    /// Show `picture` as `cmd` asks: a virtual placement for the program's own placeholders, or
    /// placeholders written at the cursor.
    fn place<T: EventListener>(
        &mut self,
        cmd: &Command,
        id: u32,
        picture: Arc<Picture>,
        term: &mut Term<T>,
    ) -> Result<(), String> {
        if id == 0 && cmd.virtual_ {
            return Err("EINVAL:a placeholder needs an image id".into());
        }
        // A part of the image is a new picture: it counts in the quota like any.
        let picture = crop(picture, cmd, &self.shared, &mut self.budget)?;
        self.make_room()?;
        let (cols, rows) = cells_for(&picture, cmd.cols, cmd.rows, self.shared.cell());
        let stamp = self.tick();
        if cmd.virtual_ {
            let kid = *self.virtual_ids.entry(id).or_insert_with(|| self.shared.new_id());
            let shown = self.shown.entry(kid).or_insert_with(|| Shown {
                picture: picture.clone(),
                placements: Vec::new(),
                generation: 0,
                origin: id,
                direct: false,
                alt: false,
                stamp,
            });
            shown.picture = picture;
            shown.placements.retain(|(p, _, _)| *p != cmd.placement);
            shown.placements.push((cmd.placement, cols, rows));
            if shown.placements.len() > MAX_PLACEMENTS {
                shown.placements.remove(0);
            }
            shown.generation += 1;
            shown.stamp = stamp;
            self.limit_shown();
            self.shared.changed();
            return Ok(());
        }
        // A named placement placed again moves: the old one goes.
        if id != 0
            && cmd.placement != 0
            && let Some(old) = self.named.remove(&(id, cmd.placement))
        {
            self.remove_shown(old);
        }
        self.sweep(term);
        let kid = self.shared.new_id();
        let alt = term.mode().contains(TermMode::ALT_SCREEN);
        let drawn = write_cells(term, kid, cols, rows, if cmd.no_move { After::Stay } else { After::Right });
        self.shown.insert(
            kid,
            Shown {
                picture,
                placements: vec![(0, drawn.0, drawn.1)],
                generation: 0,
                origin: id,
                direct: true,
                alt,
                stamp,
            },
        );
        if id != 0 && cmd.placement != 0 {
            self.named.insert((id, cmd.placement), kid);
        }
        self.limit_shown();
        self.shared.changed();
        Ok(())
    }

    fn delete<T: EventListener>(&mut self, cmd: &Command, term: &Term<T>) {
        // A transmission still arriving is abandoned.
        self.pending = None;
        let free = cmd.delete.is_ascii_uppercase();
        match cmd.delete.to_ascii_lowercase() {
            b'a' => {
                self.shown.retain(|_, s| !s.direct);
                self.named.clear();
                if free {
                    let kept: Vec<u32> = self.shown.values().map(|s| s.origin).collect();
                    self.uploads.retain(|id, _| kept.contains(id));
                }
            }
            b'i' => {
                let place = (cmd.placement != 0).then_some(cmd.placement);
                self.forget_origin(cmd.id, place, free);
            }
            b'n' => {
                if let Some(&id) = self.numbers.get(&cmd.number) {
                    let place = (cmd.placement != 0).then_some(cmd.placement);
                    self.forget_origin(id, place, free);
                }
            }
            b'r' => {
                let ids: Vec<u32> = self
                    .uploads
                    .keys()
                    .chain(self.virtual_ids.keys())
                    .copied()
                    .filter(|id| (cmd.x..=cmd.y).contains(id))
                    .collect();
                for id in ids {
                    self.forget_origin(id, None, free);
                }
            }
            // Placements under a cell, a column or a row of the screen.
            b'c' | b'p' | b'q' | b'x' | b'y' => {
                let cursor = term.grid().cursor.point;
                let (cols, lines) = (term.columns(), term.screen_lines());
                let cells: Vec<(usize, usize)> = match cmd.delete.to_ascii_lowercase() {
                    b'c' => vec![(cursor.column.0, cursor.line.0.max(0) as usize)],
                    b'x' => (0..lines).map(|l| (cmd.x.saturating_sub(1) as usize, l)).collect(),
                    b'y' => (0..cols).map(|c| (c, cmd.y.saturating_sub(1) as usize)).collect(),
                    _ => vec![(cmd.x.saturating_sub(1) as usize, cmd.y.saturating_sub(1) as usize)],
                };
                for (col, line) in cells {
                    if col >= cols || line >= lines {
                        continue;
                    }
                    let cell = &term.grid()[Line(line as i32)][Column(col)];
                    if let Some(p) = Placeholder::of_cell(cell)
                        && self.shown.get(&p.id).is_some_and(|s| s.direct)
                    {
                        self.remove_shown(p.id);
                    }
                }
            }
            _ => {}
        }
        self.shared.changed();
    }

    /// What shows the program's image `id` goes (only placement `place` when given); with
    /// `free`, its data too.
    fn forget_origin(&mut self, id: u32, place: Option<u32>, free: bool) {
        match place {
            Some(p) => {
                if let Some(kid) = self.named.remove(&(id, p)) {
                    self.remove_shown(kid);
                }
                if let Some(&kid) = self.virtual_ids.get(&id)
                    && let Some(s) = self.shown.get_mut(&kid)
                {
                    s.placements.retain(|(pid, _, _)| *pid != p);
                    s.generation += 1;
                    // Without placements nothing shows it: it goes, from the terminals too.
                    if s.placements.is_empty() {
                        self.remove_shown(kid);
                    }
                }
            }
            None => {
                self.shown.retain(|_, s| s.origin != id);
                self.named.retain(|(i, _), _| *i != id);
                self.virtual_ids.remove(&id);
            }
        }
        if free {
            self.uploads.remove(&id);
            self.numbers.retain(|_, i| *i != id);
        }
        self.shared.changed();
    }

    /// Keep the images of all panes within the quota, the new one counted: this pane's oldest
    /// images go first. Another pane's images are not this pane's to drop.
    fn make_room(&mut self) -> Result<(), String> {
        while self.shared.used() > self.shared.quota {
            let oldest_upload = self.uploads.iter().min_by_key(|(_, u)| u.stamp).map(|(id, u)| (u.stamp, *id));
            let oldest_shown = self.shown.iter().min_by_key(|(_, s)| s.stamp).map(|(id, s)| (s.stamp, *id));
            match (oldest_upload, oldest_shown) {
                (Some((a, id)), Some((b, _))) if a <= b => self.remove_upload(id),
                (_, Some((_, kid))) => self.remove_shown(kid),
                (Some((_, id)), None) => self.remove_upload(id),
                (None, None) => return Err("ENOSPC:image storage is full".into()),
            }
        }
        Ok(())
    }

    /// An image on show goes, with the names that led to it.
    fn remove_shown(&mut self, kid: u32) {
        if self.shown.remove(&kid).is_some() {
            self.named.retain(|_, k| *k != kid);
            self.virtual_ids.retain(|_, k| *k != kid);
            self.shared.changed();
        }
    }

    /// An image kept for later placements goes, with its number.
    fn remove_upload(&mut self, id: u32) {
        self.uploads.remove(&id);
        self.numbers.retain(|_, i| *i != id);
    }

    /// No more than [`MAX_SHOWN`] images on show: the oldest go.
    fn limit_shown(&mut self) {
        while self.shown.len() > MAX_SHOWN {
            let Some(oldest) = self.shown.iter().min_by_key(|(_, s)| s.stamp).map(|(k, _)| *k) else { return };
            self.remove_shown(oldest);
        }
    }

    /// Whether the next image placed looks for those whose cells are gone (see `sweep`): only
    /// then are those of the archived history needed.
    pub fn sweep_due(&self) -> bool {
        self.shown.values().filter(|s| s.direct).count() >= self.sweep_at
    }

    /// Images placed at the cursor whose cells are all gone (written over, or out of the
    /// history) are forgotten. kraal reads the grid through each time their number doubles, so
    /// the cost stays in proportion to what was placed. The images of the screen that is not up
    /// (main or alternate) are left alone, except that the alternate screen's go with it.
    fn sweep<T: EventListener>(&mut self, term: &Term<T>) {
        if !self.sweep_due() {
            return;
        }
        let alt = term.mode().contains(TermMode::ALT_SCREEN);
        let mut seen = placed_ids(term);
        if !alt {
            seen.extend(&self.archived);
        }
        let gone: Vec<u32> = self
            .shown
            .iter()
            .filter(|(kid, s)| s.direct && ((s.alt == alt && !seen.contains(kid)) || (s.alt && !alt)))
            .map(|(kid, _)| *kid)
            .collect();
        for kid in gone {
            self.remove_shown(kid);
        }
        self.sweep_at = (2 * self.shown.values().filter(|s| s.direct).count()).max(SWEEP_START);
    }

    // ---- sixel ---------------------------------------------------------------------------

    fn sixel<T: EventListener>(&mut self, data: &[u8], term: &mut Term<T>) {
        if !self.shared.available() || self.budget.spent() {
            return;
        }
        let Some(img) = sixel::decode(data) else { return };
        self.budget.spend(u64::from(img.width) * u64::from(img.height));
        let Ok(picture) = Picture::from_rgba(img.width, img.height, img.pixels, &self.shared) else { return };
        if self.make_room().is_err() {
            return;
        }
        self.sweep(term);
        let (cols, rows) = cells_for(&picture, 0, 0, self.shared.cell());
        let kid = self.shared.new_id();
        let alt = term.mode().contains(TermMode::ALT_SCREEN);
        let drawn = write_cells(term, kid, cols, rows, After::NextLine);
        let stamp = self.tick();
        self.shown.insert(
            kid,
            Shown {
                picture,
                placements: vec![(0, drawn.0, drawn.1)],
                generation: 0,
                origin: 0,
                direct: true,
                alt,
                stamp,
            },
        );
        self.limit_shown();
        self.shared.changed();
    }

    // ---- showing the cells ---------------------------------------------------------------

    /// Kraal's id for the image a placeholder cell names, when it is on show here.
    fn resolve(&self, id: u32) -> Option<u32> {
        if let Some(kid) = self.virtual_ids.get(&id) {
            return self.shown.contains_key(kid).then_some(*kid);
        }
        self.shown.get(&id).is_some_and(|s| s.direct).then_some(id)
    }

    /// Turn the placeholder cells of `area` (just rendered from this pane) into cells the
    /// outer terminal shows as the image, or into blanks when it cannot (`show` false) or the
    /// image is gone.
    pub fn show_cells(&self, area: Rect, buf: &mut Buffer, show: bool) {
        for y in area.top()..area.bottom() {
            // Cells without diacritics continue the cell on their left (kitty's rules).
            let mut left: Option<Placeholder> = None;
            for x in area.left()..area.right() {
                let Some(cell) = buf.cell_mut((x, y)) else { continue };
                let Some(mut p) = Placeholder::of_symbol(cell.symbol(), cell.fg) else {
                    left = None;
                    continue;
                };
                p.infer(left.as_ref(), cell.underline_color);
                left = Some(p.clone());
                match self.resolve(p.id).filter(|_| show) {
                    Some(kid) => {
                        cell.set_symbol(&placeholder(kid, p.row, p.col));
                        cell.fg = Color::Rgb((kid >> 16) as u8, (kid >> 8) as u8, kid as u8);
                    }
                    None => {
                        cell.set_symbol(" ");
                        cell.fg = Color::Reset;
                        cell.underline_color = Color::Reset;
                    }
                }
            }
        }
    }

    /// A reply to a program's request for the device attributes: sixel among them when
    /// images can be shown.
    pub fn device_attributes(&self, reply: &str) -> String {
        if reply == "\x1b[?6c" && self.shared.available() { "\x1b[?62;4;22c".into() } else { reply.to_string() }
    }
}

/// Write the placeholder cells of image `kid`, `cols × rows`, with their top left at `at`;
/// only those inside `clip`.
pub fn write_placeholders(buf: &mut Buffer, clip: Rect, at: (u16, u16), kid: u32, cols: u16, rows: u16) {
    let clip = clip.intersection(buf.area);
    for row in 0..rows {
        for col in 0..cols {
            let (x, y) = (at.0.saturating_add(col), at.1.saturating_add(row));
            if !clip.contains((x, y).into()) {
                continue;
            }
            let cell = &mut buf[(x, y)];
            cell.reset();
            cell.set_symbol(&placeholder(kid, usize::from(row), usize::from(col)));
            cell.fg = Color::Rgb((kid >> 16) as u8, (kid >> 8) as u8, kid as u8);
        }
    }
}

/// The text of a placeholder cell for kraal's image `kid`: row, column and the id's high byte.
pub fn placeholder(kid: u32, row: usize, col: usize) -> String {
    let mut s = String::with_capacity(16);
    s.push(PLACEHOLDER);
    s.push(DIACRITICS[row.min(MAX_CELLS - 1)]);
    s.push(DIACRITICS[col.min(MAX_CELLS - 1)]);
    s.push(DIACRITICS[(kid >> 24) as usize % MAX_CELLS]);
    s
}

fn diacritic_index(c: char) -> Option<usize> {
    DIACRITICS.iter().position(|d| *d == c)
}

/// The image a placeholder cell of a pane's grid shows, by kraal's id.
pub fn image_of_cell(cell: &Cell) -> Option<u32> {
    Placeholder::of_cell(cell).map(|p| p.id)
}

/// The images whose placeholder cells are in the grid of the screen that is up, history
/// included.
fn placed_ids<T: EventListener>(term: &Term<T>) -> HashSet<u32> {
    let grid = term.grid();
    let mut ids = HashSet::new();
    for line in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
        let row = &grid[Line(line)];
        for col in 0..grid.columns() {
            let cell = &row[Column(col)];
            if cell.c == PLACEHOLDER
                && let Some(p) = Placeholder::of_cell(cell)
            {
                ids.insert(p.id);
            }
        }
    }
    ids
}

/// What a placeholder cell says: the image id (colour and third diacritic), row and column.
#[derive(Debug, Clone, PartialEq)]
struct Placeholder {
    id: u32,
    row: usize,
    col: usize,
    /// Which of row, column and high byte the cell spelled out.
    given: (bool, bool, bool),
    /// The id from the colour alone, and the underline (placement) colour, to compare with
    /// the cell on the left.
    low: u32,
    underline: Option<Color>,
}

impl Placeholder {
    fn of_symbol(symbol: &str, fg: Color) -> Option<Placeholder> {
        let mut chars = symbol.chars();
        if chars.next()? != PLACEHOLDER {
            return None;
        }
        let low = match fg {
            Color::Rgb(r, g, b) => u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
            Color::Indexed(i) => u32::from(i),
            _ => 0,
        };
        let marks: Vec<usize> = chars.filter_map(diacritic_index).take(3).collect();
        let high = marks.get(2).map_or(0, |h| (*h as u32) << 24);
        Some(Placeholder {
            id: low | high,
            row: marks.first().copied().unwrap_or(0),
            col: marks.get(1).copied().unwrap_or(0),
            given: (!marks.is_empty(), marks.len() > 1, marks.len() > 2),
            low,
            underline: None,
        })
    }

    fn of_cell(cell: &Cell) -> Option<Placeholder> {
        if cell.c != PLACEHOLDER {
            return None;
        }
        let fg = match cell.fg {
            AColor::Spec(Rgb { r, g, b }) => Color::Rgb(r, g, b),
            AColor::Indexed(i) => Color::Indexed(i),
            _ => Color::Reset,
        };
        let mut s = String::new();
        s.push(cell.c);
        if let Some(zw) = cell.zerowidth() {
            s.extend(zw);
        }
        Placeholder::of_symbol(&s, fg)
    }

    /// Fill in what the cell left out from the cell on its left.
    fn infer(&mut self, left: Option<&Placeholder>, underline: Color) {
        self.underline = Some(underline);
        let Some(l) = left.filter(|l| l.low == self.low && l.underline == self.underline) else { return };
        match self.given {
            (false, _, _) => {
                self.row = l.row;
                self.col = l.col + 1;
                self.id = self.low | (l.id & 0xff00_0000);
            }
            (true, false, _) if l.row == self.row => {
                self.col = l.col + 1;
                self.id = self.low | (l.id & 0xff00_0000);
            }
            (true, true, false) if l.row == self.row && l.col + 1 == self.col => {
                self.id = self.low | (l.id & 0xff00_0000);
            }
            _ => {}
        }
    }
}

/// The part of the picture a command asks for (`x`, `y`, `w`, `h` in pixels).
fn crop(
    picture: Arc<Picture>,
    cmd: &Command,
    shared: &Arc<Shared>,
    budget: &mut Budget,
) -> Result<Arc<Picture>, String> {
    let (x, y) = (cmd.x.min(picture.width), cmd.y.min(picture.height));
    let w = if cmd.w == 0 { picture.width - x } else { cmd.w.min(picture.width - x) };
    let h = if cmd.h == 0 { picture.height - y } else { cmd.h.min(picture.height - y) };
    if (x, y, w, h) == (0, 0, picture.width, picture.height) {
        return Ok(picture);
    }
    if w == 0 || h == 0 {
        return Err("EINVAL:empty source rectangle".into());
    }
    // The whole picture is decoded for any part of it.
    if !budget.spend(u64::from(picture.width) * u64::from(picture.height)) {
        return Err(BUSY.into());
    }
    let part = picture.decode()?.crop_imm(x, y, w, h).to_rgba8();
    Picture::from_rgba(w, h, part.into_raw(), shared)
}

/// The cells a picture spans: as asked, the other side keeping its proportions when only one
/// is given, or its size in the outer terminal's cells.
fn cells_for(picture: &Picture, cols: u32, rows: u32, cell: (u16, u16)) -> (u16, u16) {
    let (cw, ch) = (f64::from(cell.0.max(1)), f64::from(cell.1.max(1)));
    let (w, h) = (f64::from(picture.width.max(1)), f64::from(picture.height.max(1)));
    let (c, r) = match (cols, rows) {
        (0, 0) => ((w / cw).ceil(), (h / ch).ceil()),
        (c, 0) => (f64::from(c), (f64::from(c) * cw * h / w / ch).ceil()),
        (0, r) => ((f64::from(r) * ch * w / h / cw).ceil(), f64::from(r)),
        (c, r) => (f64::from(c), f64::from(r)),
    };
    let fit = |v: f64| v.clamp(1.0, MAX_CELLS as f64) as u16;
    (fit(c), fit(r))
}

/// Where the cursor goes after an image.
#[derive(Clone, Copy, PartialEq)]
enum After {
    /// After the image's last column, on its last row (kitty).
    Right,
    /// Where it was.
    Stay,
    /// The line below the image, in the column it started (sixel).
    NextLine,
}

/// Write the placeholder cells of image `kid` at the cursor, `cols × rows`, cut at the right
/// edge of the screen; returns the cells written.
fn write_cells<T: EventListener>(term: &mut Term<T>, kid: u32, cols: u16, rows: u16, after: After) -> (u16, u16) {
    let start = term.grid().cursor.point;
    let start_col = start.column.0;
    let drawn = (cols as usize).min(term.columns().saturating_sub(start_col)).max(1);
    let rows = rows as usize;
    let saved = term.grid().cursor.template.clone();
    term.grid_mut().cursor.template =
        Cell { fg: AColor::Spec(Rgb { r: (kid >> 16) as u8, g: (kid >> 8) as u8, b: kid as u8 }), ..Cell::default() };
    for row in 0..rows {
        if row > 0 {
            term.linefeed();
            term.goto_col(start_col);
        }
        for col in 0..drawn {
            for c in placeholder(kid, row, col).chars() {
                term.input(c);
            }
        }
    }
    term.grid_mut().cursor.template = saved;
    match after {
        After::Right => {}
        After::Stay => {
            // Scrolling moved the image up: the cursor goes back to its top left.
            let line = term.grid().cursor.point.line.0 - (rows as i32 - 1);
            term.goto(line.max(0), start_col);
        }
        After::NextLine => {
            term.linefeed();
            term.goto_col(start_col);
        }
    }
    (drawn as u16, rows as u16)
}

/// A kitty graphics command, parsed.
#[derive(Debug, Clone, Default, PartialEq)]
struct Command {
    /// `a`, when given (the default is to transmit).
    action: Option<u8>,
    quiet: u32,
    format: u32,
    medium: u8,
    width: u32,
    height: u32,
    id: u32,
    number: u32,
    placement: u32,
    compressed: bool,
    more: bool,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    cols: u32,
    rows: u32,
    no_move: bool,
    virtual_: bool,
    delete: u8,
    payload: Vec<u8>,
}

impl Command {
    fn parse(body: &[u8]) -> Result<Command, String> {
        let (control, payload) = match body.iter().position(|b| *b == b';') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &[][..]),
        };
        let mut cmd =
            Command { format: 32, medium: b'd', delete: b'a', payload: payload.to_vec(), ..Default::default() };
        for pair in control.split(|b| *b == b',').filter(|p| !p.is_empty()) {
            let [key, b'=', value @ ..] = pair else { return Err("EINVAL:bad control data".into()) };
            let number = || -> Result<u32, String> {
                std::str::from_utf8(value)
                    .ok()
                    .and_then(|v| v.parse::<i64>().ok())
                    .map(|v| v.clamp(0, i64::from(u32::MAX)) as u32)
                    .ok_or_else(|| "EINVAL:bad number".to_string())
            };
            let letter = || value.first().copied().ok_or_else(|| "EINVAL:empty value".to_string());
            match key {
                b'a' => cmd.action = Some(letter()?),
                b'q' => cmd.quiet = number()?,
                b'f' => cmd.format = number()?,
                b't' => cmd.medium = letter()?,
                b's' => cmd.width = number()?,
                b'v' => cmd.height = number()?,
                b'i' => cmd.id = number()?,
                b'I' => cmd.number = number()?,
                b'p' => cmd.placement = number()?,
                b'o' => cmd.compressed = letter()? == b'z',
                b'm' => cmd.more = number()? == 1,
                b'x' => cmd.x = number()?,
                b'y' => cmd.y = number()?,
                b'w' => cmd.w = number()?,
                b'h' => cmd.h = number()?,
                b'c' => cmd.cols = number()?,
                b'r' => cmd.rows = number()?,
                b'C' => cmd.no_move = number()? == 1,
                b'U' => cmd.virtual_ = number()? == 1,
                b'd' => cmd.delete = letter()?,
                // Offsets within a cell, z-index, parents, sizes for files: not used here.
                _ => {}
            }
        }
        Ok(cmd)
    }

    /// The reply to send the program about image `id`, unless it asked for quiet or gave no
    /// id or number to reply to.
    fn reply(&self, id: u32, msg: String) -> Option<Vec<u8>> {
        let ok = msg == "OK";
        if (ok && self.quiet >= 1) || (!ok && self.quiet >= 2) || (id == 0 && self.number == 0) {
            return None;
        }
        let mut keys = format!("i={id}");
        if self.number != 0 {
            keys.push_str(&format!(",I={}", self.number));
        }
        if self.placement != 0 {
            keys.push_str(&format!(",p={}", self.placement));
        }
        // Only printable ASCII goes back.
        let msg: String = msg.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(200).collect();
        Some(format!("\x1b_G{keys};{msg}\x1b\\").into_bytes())
    }
}

// ---- commands for the outer terminal ---------------------------------------------------

/// Send image `kid` to the outer terminal (PNG, in chunks), without showing it.
pub fn transmit(kid: u32, picture: &Picture, out: &mut Vec<u8>) {
    let data = base64::engine::general_purpose::STANDARD.encode(&picture.png);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            out.extend_from_slice(format!("\x1b_Ga=t,f=100,i={kid},q=2,m={more};").as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more},q=2;").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// The virtual placement placeholder cells of image `kid` refer to.
pub fn place(kid: u32, placement: u32, cols: u16, rows: u16, out: &mut Vec<u8>) {
    let p = if placement != 0 { format!(",p={placement}") } else { String::new() };
    out.extend_from_slice(format!("\x1b_Ga=p,U=1,i={kid}{p},c={cols},r={rows},q=2\x1b\\").as_bytes());
}

/// Forget image `kid` in the outer terminal.
pub fn delete(kid: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("\x1b_Ga=d,d=I,i={kid},q=2\x1b\\").as_bytes());
}

/// Remove the placements of image `kid` in the outer terminal, keeping the image.
pub fn unplace(kid: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("\x1b_Ga=d,d=i,i={kid},q=2\x1b\\").as_bytes());
}

/// An image the panes show, for a client's terminal: the generation of its placements, the
/// picture, and the placements (placement id, columns, rows).
pub type Live = (u64, Arc<Picture>, Vec<(u32, u16, u16)>);

/// Bring a terminal that has the images of `sent` (with the generation of their placements) up
/// to `live`, writing the commands to `out`: images no pane shows go, new ones are sent, and an
/// image whose placements changed loses them all and gets the current ones, so that a placement
/// removed in kraal goes from the terminal too. New images stop at `budget` bytes, one large
/// image excepted; true when some had to wait.
pub fn update_terminal(
    sent: &mut HashMap<u32, u64>,
    live: HashMap<u32, Live>,
    budget: usize,
    out: &mut Vec<u8>,
) -> bool {
    let gone: Vec<u32> = sent.keys().filter(|k| !live.contains_key(k)).copied().collect();
    for kid in gone {
        delete(kid, out);
        sent.remove(&kid);
    }
    let mut more = false;
    for (kid, (generation, picture, placements)) in live {
        match sent.get(&kid) {
            Some(had) if *had == generation => continue,
            Some(_) => unplace(kid, out),
            None => {
                if !out.is_empty() && out.len() + picture.png.len() * 4 / 3 > budget {
                    more = true;
                    continue;
                }
                transmit(kid, &picture, out);
            }
        }
        for (placement, cols, rows) in placements {
            place(kid, placement, cols, rows, out);
        }
        sent.insert(kid, generation);
    }
    more
}

/// The query that tells whether the outer terminal shows images (its answer carries
/// [`PROBE_ID`]), the requests for its name and version (XTVERSION), for its cell size in
/// pixels, and for its device attributes (sixel among them), which every terminal answers last.
pub const PROBE_ID: u32 = 0x4b52_4c31;

pub fn probe() -> String {
    format!("\x1b_Gi={PROBE_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[>q\x1b[16t\x1b[c")
}

/// Whether a terminal's device attributes (`CSI ? … c`) say it draws sixel (attribute 4).
pub fn sixel_answer(reply: &str) -> Option<bool> {
    let body = reply.strip_prefix("\x1b[?")?.strip_suffix('c')?;
    if !body.bytes().all(|b| b.is_ascii_digit() || b == b';') {
        return None;
    }
    Some(body.split(';').any(|a| a == "4"))
}

/// The terminal's name in a reply to XTVERSION (`DCS > | name ST`).
pub fn terminal_name(reply: &str) -> Option<String> {
    let body = reply.strip_prefix("\x1bP>|")?;
    let body = body.strip_suffix("\x1b\\").or_else(|| body.strip_suffix('\x07')).unwrap_or(body);
    Some(body.chars().filter(|c| !c.is_control()).take(100).collect())
}

/// Whether a terminal draws images over Unicode placeholders: answering the graphics query is
/// not enough (WezTerm and Konsole show images, but not this way, and would print the
/// placeholder characters).
pub fn draws_placeholders(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("kitty") || name.starts_with("ghostty")
}

/// What a terminal reply says about images: `Some(true)` when it is the answer to the probe
/// saying they work, `Some(false)` for any other graphics reply.
pub fn probe_answer(reply: &str) -> Option<bool> {
    let body = reply.strip_prefix("\x1b_G")?;
    Some(body.starts_with(&format!("i={PROBE_ID};OK")))
}

/// The cell size in a reply to `CSI 16 t` (`CSI 6 ; height ; width t`).
pub fn cell_size_answer(reply: &str) -> Option<(u16, u16)> {
    let body = reply.strip_prefix("\x1b[6;")?.strip_suffix('t')?;
    let (h, w) = body.split_once(';')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}
