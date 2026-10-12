//! Images for terminals that draw sixel but not kitty's placeholders. The placeholder cells a
//! frame shows (once [`super::PaneGraphics::show_cells`] resolved them) become blanks, and each
//! rectangle of an image's cells is drawn over them as a sixel image of that part alone. What
//! was drawn is remembered: a part is drawn again only when it moves, changes or is uncovered,
//! and the cells of a part that went are erased. An image that moves (output scrolling under
//! it) is erased at once and drawn again once it stays put: sending it at each step would cost
//! the server and the terminal far more than the text.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use super::Placeholder;

/// A rectangle of an image's cells on screen: `area` shows the cells of placement `placement`
/// of image `kid` from column `col` and row `row` of the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    pub kid: u32,
    pub placement: u32,
    pub area: Rect,
    pub col: u16,
    pub row: u16,
}

/// Turn the image cells of a frame into blanks (the image goes over them) and return the
/// rectangles they form within `within`; those outside are left blank.
pub fn take_parts(buf: &mut Buffer, within: Rect) -> Vec<Part> {
    let area = buf.area;
    let mut done = Vec::new();
    // Rectangles that reach the row above, to be continued by the runs of this one.
    let mut open: Vec<Part> = Vec::new();
    for y in area.top()..area.bottom() {
        let mut runs: Vec<Part> = Vec::new();
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            let Some(p) = Placeholder::of_symbol(cell.symbol(), cell.fg) else { continue };
            let placement = color_id(cell.underline_color);
            cell.set_symbol(" ");
            cell.fg = Color::Reset;
            cell.underline_color = Color::Reset;
            if !within.contains((x, y).into()) {
                continue;
            }
            let (col, row) = (p.col.min(usize::from(u16::MAX)) as u16, p.row.min(usize::from(u16::MAX)) as u16);
            match runs.last_mut() {
                Some(r)
                    if r.kid == p.id
                        && r.placement == placement
                        && r.area.right() == x
                        && r.row == row
                        && r.col.checked_add(r.area.width) == Some(col) =>
                {
                    r.area.width += 1
                }
                _ => runs.push(Part { kid: p.id, placement, area: Rect::new(x, y, 1, 1), col, row }),
            }
        }
        let mut next = Vec::with_capacity(runs.len());
        for run in runs {
            let above = open.iter().position(|o| {
                (o.kid, o.placement, o.area.x, o.area.width, o.col)
                    == (run.kid, run.placement, run.area.x, run.area.width, run.col)
                    && o.area.bottom() == y
                    && o.row.checked_add(o.area.height) == Some(run.row)
            });
            match above {
                Some(i) => {
                    let mut o = open.swap_remove(i);
                    o.area.height += 1;
                    next.push(o);
                }
                None => next.push(run),
            }
        }
        done.append(&mut open);
        open = next;
    }
    done.append(&mut open);
    done
}

/// The id a colour spells (placement ids are given as the underline colour).
fn color_id(c: Color) -> u32 {
    match c {
        Color::Rgb(r, g, b) => u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
        Color::Indexed(i) => u32::from(i),
        _ => 0,
    }
}

/// A part as drawn, with what its pixels depend on besides where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Drawn {
    pub part: Part,
    /// The picture, by its address (it stays the same while the picture lives).
    pub picture: usize,
    /// The placement's size in cells, and the cell size in pixels.
    pub cells: (u16, u16),
    pub cell: (u16, u16),
}

/// What a terminal shows of the images.
#[derive(Default)]
pub struct Painted {
    drawn: Vec<Drawn>,
}

impl Painted {
    /// The terminal was cleared (or may have lost what it was sent): nothing is on it.
    pub fn clear(&mut self) {
        self.drawn.clear();
    }

    /// The areas of the parts drawn that the next frame does not show as they are: their
    /// cells are erased.
    pub fn stale(&self, wanted: &[Drawn]) -> Vec<Rect> {
        self.drawn.iter().filter(|d| !wanted.contains(d)).map(|d| d.part.area).collect()
    }

    /// The parts of `wanted` to draw: those not on the terminal yet, and those the frame
    /// writes text over (`rewritten` says whether it writes to an area), which takes them off.
    /// `wanted` is on the terminal afterwards.
    pub fn update(&mut self, wanted: Vec<Drawn>, rewritten: impl Fn(Rect) -> bool) -> Vec<Drawn> {
        let draw = wanted.iter().filter(|d| !self.drawn.contains(d) || rewritten(d.part.area)).cloned().collect();
        self.drawn = wanted;
        draw
    }

    /// `part` is not on the terminal after all (its image was not ready).
    pub fn forget(&mut self, part: &Drawn) {
        self.drawn.retain(|d| d != part);
    }

    pub fn is_empty(&self) -> bool {
        self.drawn.is_empty()
    }
}

/// Where each image's top left was on the screen in the last frame, and since when the images
/// that moved have been moving.
#[derive(Default)]
pub struct Motion {
    origins: HashMap<u32, Vec<(i32, i32)>>,
    moved: HashMap<u32, Instant>,
}

impl Motion {
    /// Note where the images of `wanted` are at `now`, and return those that moved less than
    /// `still` ago, with when they may be drawn. An image that is covered or uncovered, or
    /// changes in place, does not move: only its top left counts.
    pub fn track(&mut self, wanted: &[Drawn], now: Instant, still: Duration) -> HashMap<u32, Instant> {
        let mut origins: HashMap<u32, Vec<(i32, i32)>> = HashMap::new();
        for d in wanted {
            let p = &d.part;
            let at = (i32::from(p.area.x) - i32::from(p.col), i32::from(p.area.y) - i32::from(p.row));
            origins.entry(p.kid).or_default().push(at);
        }
        for (kid, at) in &mut origins {
            at.sort_unstable();
            at.dedup();
            if self.origins.get(kid).is_some_and(|before| before != at) {
                self.moved.insert(*kid, now);
            }
        }
        self.moved.retain(|kid, since| origins.contains_key(kid) && now.duration_since(*since) < still);
        self.origins = origins;
        self.moved.iter().map(|(kid, since)| (*kid, *since + still)).collect()
    }
}

/// Erase the cells of `area`, row by row (ECH): erasing takes a sixel image off in every
/// terminal, writing text over it not in all.
pub fn erase(area: Rect, out: &mut Vec<u8>) {
    for y in area.top()..area.bottom() {
        let _ = write!(out, "\x1b[{};{}H\x1b[{}X", y + 1, area.x + 1, area.width);
    }
}

/// The pixels of a part in its image fitted to the placement, with cells of `cell` pixels:
/// x, y, width, height. The height stops at the last whole sixel band (six rows of pixels), so
/// that nothing reaches the row below: a terminal that puts its cursor under an image would
/// scroll at the bottom of the screen.
pub fn pixels(part: &Part, cell: (u16, u16)) -> (u32, u32, u32, u32) {
    let (cw, ch) = (u32::from(cell.0), u32::from(cell.1));
    let mut h = u32::from(part.area.height) * ch;
    if h >= 6 {
        h -= h % 6;
    }
    (u32::from(part.col) * cw, u32::from(part.row) * ch, u32::from(part.area.width) * cw, h)
}
