//! Images for clients: whose terminal shows them and how, and bringing each one up to date with
//! the images the panes show. A terminal that draws kitty graphics over placeholder cells gets
//! the images ahead of the frames that use them; one that draws sixel gets the visible parts of
//! the images drawn over each frame, when they change.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::clients::frame_bytes;
use super::{ClientId, Server};
use crate::graphics::paint::{self, Drawn, Motion, Painted};
use crate::graphics::quantize::{self, Indexed};
use crate::graphics::{self, Picture};
use crate::layout::PaneId;

/// Image data sent with one frame at most; the rest follows with the next frames.
const BUDGET: usize = 4 << 20;
/// Pictures fitted for sixel at once, each on a thread of its own.
const FITTING: usize = 4;
/// The panes' pictures prepared at once, each on a thread of its own (a pane's one at a time).
const PREPARING: usize = 2;
/// How long an image that moved stays put before it is drawn again as sixel.
const STILL: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(super) struct ClientImages {
    /// The terminal answered the probe (it speaks the kitty graphics protocol), and its name
    /// says it draws images over placeholder cells.
    answered: bool,
    name: Option<String>,
    capable: bool,
    /// Its device attributes say it draws sixel...
    says_sixel: bool,
    /// ...and it told its cell size: images are drawn on it as sixel.
    sixel: bool,
    /// Its cell size in pixels, once it said (and since the last resize, for sixel).
    cell: Option<(u16, u16)>,
    /// Images it has, with the generation of their placements.
    sent: HashMap<u32, u64>,
    /// The images' version when it was last brought up to date.
    version: Option<u64>,
    /// The parts of images drawn on it as sixel, and the images that move.
    painted: Painted,
    motion: Motion,
}

impl ClientImages {
    /// What it was sent may not have arrived: send it all again.
    pub(super) fn forget(&mut self) {
        self.sent.clear();
        self.version = None;
        self.painted.clear();
    }

    /// What its answers so far make of it, in whatever order they came. Asking its cell size
    /// again (after a resize) does not stop sixel meanwhile.
    fn settle(&mut self) {
        self.capable |= self.answered && self.name.as_deref().is_some_and(graphics::draws_placeholders);
        self.sixel = self.says_sixel && (self.sixel || self.cell.is_some());
    }

    /// How it shows images: when that changes, it is sent all of them again.
    fn shows(&self) -> (bool, bool) {
        (self.capable, self.sixel)
    }

    /// Images are drawn as sixel there (placeholders win when it does both).
    fn draws_sixel(&self) -> bool {
        self.sixel && !self.capable
    }

    fn shows_images(&self) -> bool {
        self.capable || self.sixel
    }
}

/// Images fitted to their placements and reduced to a palette, ready to draw as sixel: the
/// most recently drawn, within a quarter of `[graphics] memory_mb`. Fitting takes a while
/// (tens of milliseconds for a large image): it is done off the server's thread.
#[derive(Default)]
pub(super) struct Fitted {
    entries: HashMap<FitKey, Fit>,
    /// Being fitted.
    pending: HashSet<FitKey>,
    bytes: usize,
    clock: u64,
}

/// A picture (by its address) and the box it is fitted to.
type FitKey = (usize, u32, u32);

struct Fit {
    /// The picture (its memory cannot be reused for another while this is held).
    picture: Weak<Picture>,
    image: Arc<Indexed>,
    used: u64,
}

/// Whether a picture is ready to draw in a box.
enum Ready {
    Now(Arc<Indexed>),
    /// Being fitted, or to be once fewer are.
    Later,
    /// To be fitted: the caller starts it.
    Start,
    /// Larger than the memory allowed.
    Never,
}

impl Fitted {
    fn key(picture: &Arc<Picture>, width: u32, height: u32) -> FitKey {
        (Arc::as_ptr(picture) as usize, width, height)
    }

    /// `picture` in a box of `width × height` pixels.
    fn ready(&mut self, picture: &Arc<Picture>, width: u32, height: u32, budget: usize) -> Ready {
        self.clock += 1;
        let key = Fitted::key(picture, width, height);
        if let Some(fit) = self.entries.get_mut(&key) {
            fit.used = self.clock;
            return Ready::Now(fit.image.clone());
        }
        if width as usize * height as usize > budget {
            return Ready::Never;
        }
        if self.pending.contains(&key) || self.pending.len() >= FITTING {
            return Ready::Later;
        }
        self.pending.insert(key);
        Ready::Start
    }

    /// A picture fitted (or that could not be: it is then drawn as nothing).
    fn done(&mut self, key: FitKey, picture: Weak<Picture>, image: Option<Indexed>, budget: usize) {
        self.pending.remove(&key);
        if picture.strong_count() == 0 {
            return;
        }
        let image = Arc::new(image.unwrap_or(Indexed { width: 0, height: 0, palette: Vec::new(), pixels: Vec::new() }));
        self.clock += 1;
        self.bytes += image.bytes();
        if let Some(old) = self.entries.insert(key, Fit { picture, image, used: self.clock }) {
            self.bytes -= old.image.bytes();
        }
        self.trim(budget);
    }

    /// Those of pictures gone go, then the least recently drawn while over `budget`.
    fn trim(&mut self, budget: usize) {
        let mut bytes = self.bytes;
        self.entries.retain(|_, f| {
            let alive = f.picture.strong_count() > 0;
            if !alive {
                bytes -= f.image.bytes();
            }
            alive
        });
        while bytes > budget && self.entries.len() > 1 {
            let Some(oldest) = self.entries.iter().min_by_key(|(_, f)| f.used).map(|(k, _)| *k) else { break };
            if let Some(f) = self.entries.remove(&oldest) {
                bytes -= f.image.bytes();
            }
        }
        self.bytes = bytes;
    }
}

impl Server {
    /// Ask a client's terminal whether it shows images, and its cell size.
    pub(super) fn probe_graphics(&mut self, id: ClientId) {
        if !self.env.graphics.enabled() {
            return;
        }
        self.images.entry(id).or_default();
        if let Some(c) = self.clients.get(&id) {
            let _ = c.out.try_send(frame_bytes(graphics::probe().as_bytes()));
        }
    }

    /// Ask again for the cell size (the font may have changed with the window). Sixel images
    /// wait for the answer: drawn with cells of another size, they would overflow theirs.
    pub(super) fn probe_cell_size(&mut self, id: ClientId) {
        if self.env.graphics.enabled()
            && let Some(c) = self.clients.get(&id)
        {
            let _ = c.out.try_send(frame_bytes(b"\x1b[16t"));
            if let Some(ci) = self.images.get_mut(&id).filter(|ci| ci.draws_sixel()) {
                ci.cell = None;
            }
        }
    }

    /// A terminal's reply about images, its name, its device attributes or its cell size;
    /// false when it is about something else.
    pub(super) fn graphics_reply(&mut self, id: ClientId, reply: &str) -> bool {
        let answer = graphics::probe_answer(reply);
        let name = graphics::terminal_name(reply);
        let sixel = graphics::sixel_answer(reply);
        if answer.is_some() || name.is_some() || sixel.is_some() {
            // Any graphics reply is the terminal's own business: never a program's input.
            let Some(ci) = self.images.get_mut(&id) else { return true };
            let before = ci.shows();
            if answer == Some(true) {
                ci.answered = true;
            }
            if name.is_some() {
                ci.name = name;
            }
            if let Some(sixel) = sixel {
                ci.says_sixel = sixel;
            }
            ci.settle();
            if ci.shows() != before {
                self.images_shown_otherwise(id);
            }
            return true;
        }
        if let Some((w, h)) = graphics::cell_size_answer(reply).filter(|(w, h)| *w > 0 && *h > 0) {
            if let Some(ci) = self.images.get_mut(&id) {
                let (before, redraw) = (ci.shows(), ci.draws_sixel() && ci.cell != Some((w, h)));
                ci.cell = Some((w, h));
                ci.settle();
                if ci.shows() != before {
                    self.images_shown_otherwise(id);
                } else if redraw {
                    self.mark_dirty_client(id);
                }
            }
            if self.primary.is_none_or(|p| p == id) && self.env.graphics.cell() != (w, h) {
                self.env.graphics.set_cell(w, h);
                for p in self.panes.values_mut() {
                    p.refresh_pixel_size();
                }
            }
            return true;
        }
        false
    }

    /// A client leaving: its terminal can forget the images it was sent.
    pub(super) fn graphics_farewell(&mut self, id: ClientId) {
        let Some(ci) = self.images.get_mut(&id) else { return };
        let mut out = Vec::new();
        for kid in ci.sent.keys() {
            graphics::delete(*kid, &mut out);
        }
        ci.forget();
        if let Some(c) = self.clients.get(&id)
            && !out.is_empty()
        {
            let _ = c.out.try_send(frame_bytes(&out));
        }
    }

    pub(super) fn graphics_client_gone(&mut self, id: ClientId) {
        if self.images.remove(&id).is_some() {
            self.update_availability();
        }
        if !self.images.values().any(ClientImages::draws_sixel) {
            self.fitted = Fitted::default();
        }
        // The primary client changed: its terminal's cell size counts now.
        if let Some(cell) = self.primary.and_then(|p| self.images.get(&p)).and_then(|ci| ci.cell) {
            self.env.graphics.set_cell(cell.0, cell.1);
        }
    }

    /// A client's terminal shows images otherwise now: it is sent all of them again, the way it
    /// shows them, and programs are told whether they can send images.
    fn images_shown_otherwise(&mut self, id: ClientId) {
        if let Some(ci) = self.images.get_mut(&id) {
            ci.forget();
        }
        self.update_availability();
        self.mark_dirty_client(id);
    }

    /// Programs may send images while an attached client can show them.
    fn update_availability(&mut self) {
        let any =
            self.images.iter().any(|(id, ci)| ci.shows_images() && self.clients.get(id).is_some_and(|c| c.attached));
        self.env.graphics.set_available(any);
    }

    /// Whether the cells of images render as images for this client.
    pub(super) fn can_show_images(&self, id: ClientId) -> bool {
        self.images.get(&id).is_some_and(ClientImages::shows_images)
    }

    /// The images the panes show, by kraal's id.
    fn live_images(&self) -> HashMap<u32, graphics::Live> {
        self.panes.values().flat_map(|p| p.images()).collect()
    }

    /// What a client's terminal needs before the next frame: images it does not have, changed
    /// placements, and the deletion of images no pane shows any more. Within a budget; true
    /// when more is left for the next frames.
    pub(super) fn graphics_updates(&mut self, id: ClientId) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let version = self.env.graphics.version();
        let Some(ci) = self.images.get(&id) else { return (out, false) };
        if !ci.capable || ci.version == Some(version) {
            return (out, false);
        }
        let live = self.live_images();
        let Some(ci) = self.images.get_mut(&id) else { return (out, false) };
        let more = graphics::update_terminal(&mut ci.sent, live, BUDGET, &mut out);
        if !more {
            ci.version = Some(version);
        }
        (out, more)
    }

    /// For a terminal that draws sixel: take the image cells out of the frame `buf` (they go
    /// blank) and return what goes with it, the erasure of the parts drawn before that are
    /// gone or moved (ahead of the text) and the parts to draw (after it), and when to draw
    /// the frame again for the images that are still moving. Parts the terminal shows as they
    /// are cost nothing.
    pub(super) fn paint_images(&mut self, id: ClientId, buf: &mut Buffer) -> (Vec<u8>, Vec<u8>, Option<Instant>) {
        let (mut erase, mut draw, mut wake) = (Vec::new(), Vec::new(), None);
        let Some(ci) = self.images.get(&id).filter(|ci| ci.draws_sixel()) else { return (erase, draw, wake) };
        let area = buf.area;
        // Never on the last row: a terminal that puts its cursor under an image would scroll.
        // Nowhere while the cell size is not known (after a resize, until the terminal says).
        let (within, cell) = match ci.cell {
            Some(cell) => (Rect { height: area.height.saturating_sub(1), ..area }, cell),
            None => (Rect::default(), (0, 0)),
        };
        let parts = paint::take_parts(buf, within);
        let buf: &Buffer = buf;
        let live = if parts.is_empty() { HashMap::new() } else { self.live_images() };
        let wanted: Vec<Drawn> = parts
            .into_iter()
            .filter_map(|part| {
                let (_, picture, placements) = live.get(&part.kid)?;
                let &(_, cols, rows) =
                    placements.iter().find(|p| p.0 == part.placement).or_else(|| placements.first())?;
                Some(Drawn { part, picture: Arc::as_ptr(picture) as usize, cells: (cols, rows), cell })
            })
            .collect();
        let (Some(c), Some(ci)) = (self.clients.get_mut(&id), self.images.get_mut(&id)) else {
            return (erase, draw, wake);
        };
        let moving = ci.motion.track(&wanted, Instant::now(), STILL);
        if wanted.is_empty() && ci.painted.is_empty() {
            return (erase, draw, wake);
        }
        if c.encoder.redraws_all(area) {
            ci.painted.clear();
        }
        for stale in ci.painted.stale(&wanted) {
            paint::erase(stale, &mut erase);
            c.encoder.redraw(stale, buf);
        }
        let todo = ci.painted.update(wanted, |r| c.encoder.rewrites(buf, r));
        let budget = self.env.graphics.quota() / 4;
        let mut start = Vec::new();
        for d in todo {
            // Moving: its cells stay blank until it stays put.
            if let Some(&at) = moving.get(&d.part.kid) {
                ci.painted.forget(&d);
                wake = Some(wake.map_or(at, |w: Instant| w.min(at)));
                continue;
            }
            let Some((_, picture, _)) = live.get(&d.part.kid) else { continue };
            let (bw, bh) = (u32::from(d.cells.0) * u32::from(cell.0), u32::from(d.cells.1) * u32::from(cell.1));
            let image = match self.fitted.ready(picture, bw, bh, budget) {
                Ready::Now(image) => image,
                Ready::Never => continue,
                // Not drawn yet: tried again with the frame that follows the fitting.
                Ready::Later => {
                    ci.painted.forget(&d);
                    continue;
                }
                Ready::Start => {
                    ci.painted.forget(&d);
                    start.push((picture.clone(), bw, bh));
                    continue;
                }
            };
            let (x, y, w, h) = paint::pixels(&d.part, cell);
            let _ = write!(draw, "\x1b[{};{}H", d.part.area.y + 1, d.part.area.x + 1);
            graphics::sixel::encode(&image, x, y, w, h, &mut draw);
        }
        for (picture, width, height) in start {
            self.fit_later(picture, width, height);
        }
        (erase, draw, wake)
    }

    /// Prepare the panes' pictures that wait for it off the loop, [`PREPARING`] at once at most.
    pub(super) fn prepare_images(&mut self) {
        while self.preparing < PREPARING {
            let Some((id, work)) = self.panes.iter_mut().find_map(|(id, p)| Some((*id, p.take_image_work()?))) else {
                return;
            };
            self.preparing += 1;
            if !self.background(work, move |server, prepared| server.image_prepared(id, prepared)) {
                self.preparing -= 1;
                if let Some(p) = self.panes.get_mut(&id) {
                    p.image_prepared(Err("ENOMEM:no thread to prepare the image".into()));
                }
                return;
            }
        }
    }

    /// A pane's picture was prepared: it shows, and the next one waiting is prepared.
    fn image_prepared(&mut self, id: PaneId, prepared: graphics::Prepared) {
        self.preparing -= 1;
        if let Some(p) = self.panes.get_mut(&id) {
            p.image_prepared(prepared);
            self.mark_dirty_pane(id);
        }
        self.prepare_images();
    }

    /// Fit `picture` in a box of `width × height` pixels on another thread; the clients are
    /// drawn again once it is ready.
    fn fit_later(&mut self, picture: Arc<Picture>, width: u32, height: u32) {
        let key = Fitted::key(&picture, width, height);
        let weak = Arc::downgrade(&picture);
        let started = self.background(
            move || picture.decode().ok().map(|i| quantize::fit(&i.into_rgba8(), width, height)),
            move |server, image| {
                let budget = server.env.graphics.quota() / 4;
                server.fitted.done(key, weak, image, budget);
                server.mark_dirty();
            },
        );
        if !started {
            self.fitted.pending.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn says_sixel(ci: &mut ClientImages) {
        ci.says_sixel = true;
        ci.settle();
    }

    fn tells_cell(ci: &mut ClientImages) {
        ci.cell = Some((10, 20));
        ci.settle();
    }

    #[test]
    fn sixel_is_drawn_once_both_answers_came_in_either_order() {
        for answers in [[says_sixel as fn(&mut ClientImages), tells_cell], [tells_cell, says_sixel]] {
            let mut ci = ClientImages::default();
            answers[0](&mut ci);
            assert!(!ci.draws_sixel(), "not on one answer");
            answers[1](&mut ci);
            assert!(ci.draws_sixel());
            // Its cell size asked again after a resize: still drawn on, once it answers.
            ci.cell = None;
            ci.settle();
            assert!(ci.draws_sixel());
        }
    }
}
