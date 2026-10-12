//! Turns composed frames into the escape sequences sent to a client's terminal.
//!
//! The server renders each client view into a [`Buffer`]; [`Encoder`] diffs it against the last
//! frame sent to that client and emits only the changed cells, wrapped in synchronized-output
//! markers so the outer terminal never shows a half-drawn frame. Cells the server knows did not
//! change ([`Kept`]) are not even compared.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use ratatui::buffer::{Buffer, Cell, CellDiffOption, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

/// What a cell to draw again holds in the previous frame: no frame holds it.
const STALE: &str = "\u{FFFF}";

/// Cursor shape requested by the focused pane (DECSCUSR values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
    pub blinking: bool,
}

/// OSC 8 hyperlinks by cell position: programs' links stay clickable in the outer terminal.
pub type Links = HashMap<(u16, u16), Arc<str>>;

/// Cells of row `y`, from column `x0` to `x1` (excluded), that hold in the frame to encode what
/// they held in the previous one: the encoder does not compare them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kept {
    pub y: u16,
    pub x0: u16,
    pub x1: u16,
}

#[derive(Default)]
pub struct Encoder {
    /// What the terminal shows: the last frame encoded, with the cells to draw again whatever
    /// they hold marked [`STALE`].
    prev: Option<Buffer>,
    prev_links: Links,
    last_cursor_style: Option<(CursorShape, bool)>,
    /// Cells were marked to draw again since the last frame: the next one compares every cell.
    redrawn: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    underline: Color,
    modifier: Modifier,
}

impl Pen {
    const RESET: Pen = Pen { fg: Color::Reset, bg: Color::Reset, underline: Color::Reset, modifier: Modifier::empty() };
}

/// Modifiers that show on a blank cell, as ratatui's diff counts them.
const VISIBLE_ON_BLANK: Modifier = Modifier::REVERSED
    .union(Modifier::UNDERLINED)
    .union(Modifier::SLOW_BLINK)
    .union(Modifier::RAPID_BLINK)
    .union(Modifier::CROSSED_OUT);

/// The widest cell the diff looks for around the cells it skips: a cell may be wider than two
/// columns (a character with halfwidth sound marks), never in practice this wide.
const WIDEST: usize = 16;

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the previous frame so the next one is drawn in full.
    pub fn invalidate(&mut self) {
        self.prev = None;
        self.prev_links.clear();
        self.last_cursor_style = None;
    }

    /// Whether the next frame of `area` is drawn whole, on a cleared screen.
    pub fn redraws_all(&self, area: Rect) -> bool {
        self.prev.as_ref().is_none_or(|p| p.area != area)
    }

    /// Whether `next` writes to a cell of `area`.
    pub fn rewrites(&self, next: &Buffer, area: Rect) -> bool {
        let Some(prev) = self.prev.as_ref().filter(|p| p.area == next.area) else { return true };
        area.intersection(next.area).positions().any(|p| prev[p] != next[p])
    }

    /// Draw the cells of `area` again with `next`, whatever they held, and on each row the
    /// cell on their left when `next` has a wide character there (its second half is in `area`).
    pub fn redraw(&mut self, area: Rect, next: &Buffer) {
        let Some(prev) = self.prev.as_mut().filter(|p| p.area == next.area) else { return };
        let area = area.intersection(prev.area);
        self.redrawn = true;
        for p in area.positions() {
            prev[p].set_symbol(STALE);
        }
        if area.x > prev.area.x {
            for y in area.top()..area.bottom() {
                let lead = (area.x - 1, y);
                if unicode_width::UnicodeWidthStr::width(next[lead].symbol()) > 1 {
                    prev[lead].set_symbol(STALE);
                }
            }
        }
    }

    /// Append the bytes that turn the previous frame into `next` to `out`, with images drawn at
    /// the cursor (sixel): `erase` goes before the cells are drawn, `images` after them, all in
    /// the same synchronized update. The cells of `kept` are taken to be unchanged since the
    /// previous frame (where they can be skipped without changing what the diff finds around
    /// them).
    #[allow(clippy::too_many_arguments)]
    pub fn encode_with_images(
        &mut self,
        next: &Buffer,
        kept: &[Kept],
        cursor: Option<Cursor>,
        links: &Links,
        erase: &[u8],
        images: &[u8],
        out: &mut Vec<u8>,
    ) {
        out.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        let full = self.redraws_all(next.area);
        let redrawn = std::mem::take(&mut self.redrawn);
        let prev = if full {
            out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
            None
        } else {
            out.extend_from_slice(erase);
            self.prev.as_ref().map(|p| p.content.as_slice())
        };
        // The cells to compare: all of them, but those kept when the previous frame is known.
        let len = next.content.len();
        let ranges = match prev {
            Some(prev) if !redrawn => compared(prev, next, kept),
            _ => vec![(0, len)],
        };
        let mut updates: Vec<(u16, u16, &Cell)> = Vec::new();
        diff_ranges(prev, next, &ranges, &mut updates);
        // Cells whose link changed but whose text did not must be drawn again too. The second
        // column of a wide character is drawn with it: its first column is redrawn instead.
        if !full && self.prev_links != *links {
            let have: std::collections::HashSet<(u16, u16)> = updates.iter().map(|(x, y, _)| (*x, *y)).collect();
            let lead = |(x, y): (u16, u16)| {
                let wide = x > 0 && unicode_width::UnicodeWidthStr::width(next[(x - 1, y)].symbol()) > 1;
                if wide { (x - 1, y) } else { (x, y) }
            };
            let mut extra: Vec<(u16, u16)> = links
                .iter()
                .filter(|(pos, uri)| self.prev_links.get(pos) != Some(uri))
                .map(|(pos, _)| *pos)
                .chain(self.prev_links.keys().filter(|pos| !links.contains_key(pos)).copied())
                .filter(|pos| next.area.contains((pos.0, pos.1).into()))
                .map(lead)
                .filter(|pos| !have.contains(pos))
                .collect();
            extra.sort_unstable();
            extra.dedup();
            updates.extend(extra.into_iter().map(|(x, y)| (x, y, &next[(x, y)])));
            updates.sort_by_key(|(x, y, _)| (*y, *x));
        }

        let mut pen = Pen::RESET;
        let mut at: Option<(u16, u16)> = None;
        let mut link: Option<&Arc<str>> = None;
        for (x, y, cell) in updates {
            // Covered by the wide character just drawn.
            if at.is_some_and(|(ax, ay)| ay == y && x < ax) {
                continue;
            }
            let want = links.get(&(x, y));
            if want != link {
                if link.is_some() {
                    out.extend_from_slice(b"\x1b]8;;\x1b\\");
                }
                if let Some(uri) = want {
                    let _ = write!(out, "\x1b]8;;{uri}\x1b\\");
                }
                link = want;
            }
            if at != Some((x, y)) {
                let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
            }
            write_style(out, &mut pen, cell);
            let width = write_symbol(out, cell);
            at = Some((x + width, y));
        }
        if link.is_some() {
            out.extend_from_slice(b"\x1b]8;;\x1b\\");
        }
        if pen != Pen::RESET {
            out.extend_from_slice(b"\x1b[0m");
        }
        if self.prev_links != *links {
            self.prev_links = links.clone();
        }
        out.extend_from_slice(images);

        if let Some(c) = cursor {
            let style = (c.shape, c.blinking);
            if self.last_cursor_style != Some(style) {
                let n = match (c.shape, c.blinking) {
                    (CursorShape::Block, true) => 1,
                    (CursorShape::Block, false) => 2,
                    (CursorShape::Underline, true) => 3,
                    (CursorShape::Underline, false) => 4,
                    (CursorShape::Bar, true) => 5,
                    (CursorShape::Bar, false) => 6,
                };
                let _ = write!(out, "\x1b[{n} q");
                self.last_cursor_style = Some(style);
            }
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", c.y + 1, c.x + 1);
        }
        out.extend_from_slice(b"\x1b[?2026l");
        // The terminal now shows `next`: only the cells compared can differ from it.
        match self.prev.as_mut() {
            Some(p) if !full => {
                for &(a, b) in &ranges {
                    p.content[a..b].clone_from_slice(&next.content[a..b]);
                }
            }
            Some(p) if p.content.len() == len => {
                p.area = next.area;
                p.content.clone_from_slice(&next.content);
            }
            _ => self.prev = Some(next.clone()),
        }
    }
}

/// The ranges of cells (as indexes in the buffers' content) to compare: everything but the
/// cells `kept`, in order. A run of kept cells is compared all the same when skipping it could
/// change what the diff finds around it: when it starts within reach of a wide cell before it,
/// in either frame, or ends within reach of one in it (ratatui's diff skips the columns a wide
/// cell covers, and redraws those of a wide cell made narrow).
fn compared(prev: &[Cell], next: &Buffer, kept: &[Kept]) -> Vec<(usize, usize)> {
    let len = next.content.len();
    let width = usize::from(next.area.width);
    let height = usize::from(next.area.height);
    let mut widths = Widths::default();
    // Whether the cell at `i` reaches no further than `end`. A symbol is never wider than its
    // length in bytes: most cells need no width.
    let mut within = |cell: &Cell, i: usize, end: usize| {
        i + cell.symbol().len().max(1) <= end || i + usize::from(widths.of(cell)).max(1) <= end
    };
    let mut skips: Vec<(usize, usize)> = kept
        .iter()
        .filter(|k| usize::from(k.y) < height && k.x0 < k.x1 && usize::from(k.x1) <= width)
        .map(|k| {
            let row = usize::from(k.y) * width;
            (row + usize::from(k.x0), row + usize::from(k.x1))
        })
        .filter(|&(a, b)| {
            (a.saturating_sub(WIDEST)..a).all(|i| within(&next.content[i], i, a) && within(&prev[i], i, a))
                && (a.max(b.saturating_sub(WIDEST))..b).all(|i| within(&next.content[i], i, b))
        })
        .collect();
    skips.sort_unstable();
    let mut out = Vec::with_capacity(skips.len() + 1);
    let mut from = 0;
    for (a, b) in skips {
        // Overlapping runs: the one already skipped counts.
        if a < from {
            from = from.max(b);
            continue;
        }
        if a > from {
            out.push((from, a));
        }
        from = b;
    }
    if from < len {
        out.push((from, len));
    }
    out
}

/// Cell widths as ratatui's diff counts them. The last symbol longer than a byte is remembered:
/// frames repeat a few (the lines of borders) over and over.
#[derive(Default)]
struct Widths {
    symbol: [u8; 16],
    len: usize,
    width: u16,
}

impl Widths {
    fn of(&mut self, cell: &Cell) -> u16 {
        let s = cell.symbol();
        if matches!(cell.diff_option, CellDiffOption::ForcedWidth(_)) {
            return cell.cell_width();
        }
        // ratatui counts a column for a one-byte symbol, none for an empty one.
        if s.len() <= 1 {
            return s.len() as u16;
        }
        if s.len() == self.len && self.symbol[..s.len()] == *s.as_bytes() {
            return self.width;
        }
        let width = cell.cell_width();
        if s.len() <= self.symbol.len() {
            self.symbol[..s.len()].copy_from_slice(s.as_bytes());
            self.len = s.len();
            self.width = width;
        }
        width
    }
}

/// Whether ratatui's diff passes over a cell without looking at it.
#[allow(deprecated)]
fn skipped(cell: &Cell) -> bool {
    matches!(cell.diff_option, CellDiffOption::Skip) || (cell.skip && matches!(cell.diff_option, CellDiffOption::None))
}

/// The cells of `next` to draw over `prev` (None: a blank screen) within `ranges` of cell
/// indexes, in order, as ratatui's `BufferDiff` finds them over the whole buffers when the cells
/// between the ranges are equal in both (see [`compared`]): a changed wide character covers the
/// columns after it, those of a wide character made narrow are drawn again when its style shows
/// on blanks, and those of an emoji with a variation selector when their text differs.
fn diff_ranges<'a>(
    prev: Option<&[Cell]>,
    next: &'a Buffer,
    ranges: &[(usize, usize)],
    out: &mut Vec<(u16, u16, &'a Cell)>,
) {
    let cells = &next.content;
    let len = cells.len();
    let blank = Cell::EMPTY;
    let old = |i: usize| prev.map_or(&blank, |p| &p[i]);
    let width = usize::from(next.area.width.max(1));
    let at = |i: usize| ((i % width) as u16 + next.area.x, (i / width) as u16 + next.area.y);
    let mut widths = Widths::default();
    let mut pos = 0;
    for &(a, b) in ranges {
        pos = pos.max(a);
        while pos < b.min(len) {
            let i = pos;
            pos += 1;
            let cur = &cells[i];
            let previous = old(i);
            match cur.diff_option {
                CellDiffOption::Skip => continue,
                _ if skipped(cur) => continue,
                CellDiffOption::ForcedWidth(w) => {
                    pos = pos.saturating_add(usize::from(w.get()).saturating_sub(1));
                    if cur != previous {
                        let (x, y) = at(i);
                        out.push((x, y, cur));
                    }
                    continue;
                }
                CellDiffOption::None | CellDiffOption::AlwaysUpdate => {}
            }
            let w = usize::from(widths.of(cur));
            if matches!(cur.diff_option, CellDiffOption::None) && cur == previous {
                pos += w.saturating_sub(1);
                continue;
            }
            let previous_w = usize::from(widths.of(previous));
            let (x, y) = at(i);
            out.push((x, y, cur));
            // The columns after it to look at: (from, to, whatever they hold).
            let trailing = if w > 1 && cur.symbol().contains('\u{FE0F}') {
                Some((i + 1, (i + w).min(len), false))
            } else if w > 1 {
                pos += w - 1;
                None
            } else if previous_w > w && (previous.bg != Color::Reset || previous.modifier.intersects(VISIBLE_ON_BLANK))
            {
                Some((i + 1, i + previous_w, true))
            } else {
                None
            };
            if let Some((mut j, mut end, force)) = trailing {
                while j < end && j < len {
                    let k = j;
                    j += usize::from(widths.of(&cells[k])).max(1);
                    end = end.max(j).min(len);
                    if !skipped(&cells[k]) && (force || old(k).symbol() != cells[k].symbol()) {
                        let (x, y) = at(k);
                        out.push((x, y, &cells[k]));
                    }
                }
                pos = end;
            }
        }
    }
}

fn write_symbol(out: &mut Vec<u8>, cell: &Cell) -> u16 {
    let sym = cell.symbol();
    if sym.is_empty() || sym.bytes().any(|b| b < 0x20 || b == 0x7f) {
        out.push(b' ');
        return 1;
    }
    out.extend_from_slice(sym.as_bytes());
    unicode_width::UnicodeWidthStr::width(sym).max(1) as u16
}

fn write_style(out: &mut Vec<u8>, pen: &mut Pen, cell: &Cell) {
    // The parameters go straight into `out`: the sequence opens with the first one.
    let start = out.len();
    let param = |out: &mut Vec<u8>| out.extend_from_slice(if out.len() == start { b"\x1b[" } else { b";" });
    if cell.modifier != pen.modifier {
        let removed = pen.modifier - cell.modifier;
        if !removed.is_empty() {
            param(out);
            out.push(b'0');
            *pen = Pen::RESET;
        }
        let added = cell.modifier - pen.modifier;
        for (m, code) in [
            (Modifier::BOLD, b'1'),
            (Modifier::DIM, b'2'),
            (Modifier::ITALIC, b'3'),
            (Modifier::UNDERLINED, b'4'),
            (Modifier::SLOW_BLINK, b'5'),
            (Modifier::RAPID_BLINK, b'6'),
            (Modifier::REVERSED, b'7'),
            (Modifier::HIDDEN, b'8'),
            (Modifier::CROSSED_OUT, b'9'),
        ] {
            if added.contains(m) {
                param(out);
                out.push(code);
            }
        }
        pen.modifier = cell.modifier;
    }
    if cell.fg != pen.fg {
        param(out);
        color_param(out, cell.fg, Layer::Fg);
        pen.fg = cell.fg;
    }
    if cell.bg != pen.bg {
        param(out);
        color_param(out, cell.bg, Layer::Bg);
        pen.bg = cell.bg;
    }
    if cell.underline_color != pen.underline {
        param(out);
        color_param(out, cell.underline_color, Layer::Underline);
        pen.underline = cell.underline_color;
    }
    if out.len() > start {
        out.push(b'm');
    }
}

#[derive(Clone, Copy)]
enum Layer {
    Fg,
    Bg,
    Underline,
}

fn named_index(c: Color) -> Option<u8> {
    Some(match c {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        _ => return None,
    })
}

/// The SGR parameters that set colour `c` on `layer`, appended to `out`.
fn color_param(out: &mut Vec<u8>, c: Color, layer: Layer) {
    let _ = match (layer, c) {
        (Layer::Fg, Color::Reset) => write!(out, "39"),
        (Layer::Bg, Color::Reset) => write!(out, "49"),
        (Layer::Underline, Color::Reset) => write!(out, "59"),
        (Layer::Fg, Color::Indexed(i)) => write!(out, "38;5;{i}"),
        (Layer::Bg, Color::Indexed(i)) => write!(out, "48;5;{i}"),
        (Layer::Underline, Color::Indexed(i)) => write!(out, "58;5;{i}"),
        (Layer::Fg, Color::Rgb(r, g, b)) => write!(out, "38;2;{r};{g};{b}"),
        (Layer::Bg, Color::Rgb(r, g, b)) => write!(out, "48;2;{r};{g};{b}"),
        (Layer::Underline, Color::Rgb(r, g, b)) => write!(out, "58;2;{r};{g};{b}"),
        (Layer::Underline, named) => write!(out, "58;5;{}", named_index(named).unwrap_or(7)),
        (layer, named) => {
            let i = named_index(named).unwrap_or(7);
            let (base, bright) = if matches!(layer, Layer::Fg) { (30, 90) } else { (40, 100) };
            if i < 8 { write!(out, "{}", base + i) } else { write!(out, "{}", bright + i - 8) }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    /// A frame without images, nothing kept from the previous one.
    fn encode(enc: &mut Encoder, next: &Buffer, cursor: Option<Cursor>, links: &Links, out: &mut Vec<u8>) {
        enc.encode_with_images(next, &[], cursor, links, &[], &[], out);
    }

    fn text(out: &[u8]) -> String {
        String::from_utf8_lossy(out).into_owned()
    }

    #[test]
    fn first_frame_is_full_and_second_is_minimal() {
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "hello", Style::default().fg(Color::Red));
        let mut enc = Encoder::new();
        let mut out = Vec::new();
        encode(&mut enc, &buf, None, &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("\x1b[2J"));
        assert!(s.contains("\x1b[31mhello"));

        buf.set_string(1, 0, "a", Style::default().fg(Color::Red));
        let mut out = Vec::new();
        encode(&mut enc, &buf, None, &Links::new(), &mut out);
        let s = text(&out);
        assert!(!s.contains("\x1b[2J"));
        assert!(s.contains("\x1b[1;2H\x1b[31ma"));
        assert!(!s.contains("llo"));
    }

    #[test]
    fn modifier_removal_resets() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "a", Style::default().add_modifier(Modifier::BOLD));
        buf.set_string(1, 0, "b", Style::default());
        let mut out = Vec::new();
        encode(&mut Encoder::new(), &buf, None, &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("\x1b[1ma\x1b[0mb"), "{s:?}");
    }

    #[test]
    fn wide_chars_advance_two_columns() {
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "日x", Style::default());
        let mut out = Vec::new();
        encode(&mut Encoder::new(), &buf, None, &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("日x"), "{s:?}");
    }

    #[test]
    fn cursor_is_placed_and_shaped_once() {
        let buf = Buffer::empty(Rect::new(0, 0, 4, 4));
        let mut enc = Encoder::new();
        let cur = Cursor { x: 2, y: 3, shape: CursorShape::Bar, blinking: false };
        let mut out = Vec::new();
        encode(&mut enc, &buf, Some(cur), &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("\x1b[6 q\x1b[4;3H\x1b[?25h"));
        let mut out = Vec::new();
        encode(&mut enc, &buf, Some(cur), &Links::new(), &mut out);
        assert!(!text(&out).contains(" q"));
    }

    #[test]
    fn hyperlinks_wrap_their_cells() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "go here", Style::default());
        let mut links = Links::new();
        let uri: Arc<str> = Arc::from("https://example.org");
        for x in 3..7 {
            links.insert((x, 0), uri.clone());
        }
        let mut enc = Encoder::new();
        let mut out = Vec::new();
        encode(&mut enc, &buf, None, &links, &mut out);
        let s = text(&out);
        assert!(s.contains("\x1b]8;;https://example.org\x1b\\\x1b[1;4Hhere\x1b]8;;\x1b\\"), "{s:?}");
        assert!(s.find("go").unwrap() < s.find("]8;;https").unwrap(), "text before the link is not linked");
        // Same text, link removed: the cells are drawn again without it.
        let mut out = Vec::new();
        encode(&mut enc, &buf, None, &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("here") && !s.contains("example.org"), "{s:?}");
    }

    #[test]
    fn a_new_link_on_a_wide_character_keeps_it_whole() {
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "日x", Style::default());
        let link = |uri: &str| {
            let uri: Arc<str> = Arc::from(uri);
            Links::from([((0, 0), uri.clone()), ((1, 0), uri)])
        };
        let mut enc = Encoder::new();
        encode(&mut enc, &buf, None, &link("https://a.example"), &mut Vec::new());
        let mut out = Vec::new();
        encode(&mut enc, &buf, None, &link("https://b.example"), &mut out);
        let s = text(&out);
        assert!(s.contains("b.example\x1b\\\x1b[1;1H日"), "{s:?}");
        assert!(!s.contains("\x1b[1;2H"), "nothing is written over its second column: {s:?}");
    }

    #[test]
    fn images_go_around_the_cells_and_erased_cells_are_drawn_again() {
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "abc", Style::default());
        let mut enc = Encoder::new();
        assert!(enc.redraws_all(area));
        let mut out = Vec::new();
        enc.encode_with_images(&buf, &[], None, &Links::new(), b"ERASE", b"IMAGE", &mut out);
        let s = text(&out);
        // The screen is cleared: nothing to erase. Images go over the cells.
        assert!(!s.contains("ERASE") && s.find("abc") < s.find("IMAGE"), "{s:?}");
        assert!(!enc.redraws_all(area) && enc.redraws_all(Rect::new(0, 0, 7, 2)));
        assert!(!enc.rewrites(&buf, area));
        // Cells to draw again (an image was erased there) are, whatever they hold.
        enc.redraw(Rect::new(1, 0, 1, 2), &buf);
        assert!(enc.rewrites(&buf, Rect::new(1, 1, 1, 1)) && !enc.rewrites(&buf, Rect::new(2, 0, 4, 2)));
        let cursor = Cursor { x: 0, y: 1, shape: CursorShape::Block, blinking: false };
        let mut out = Vec::new();
        enc.encode_with_images(&buf, &[], Some(cursor), &Links::new(), b"ERASE", b"IMAGE", &mut out);
        let s = text(&out);
        let at = |t: &str| s.find(t).unwrap_or_else(|| panic!("{t:?} in {s:?}"));
        assert!(at("\x1b[?2026h") < at("ERASE") && at("ERASE") < at("\x1b[1;2Hb") && at("b") < at("IMAGE"), "{s:?}");
        assert!(at("IMAGE") < at("\x1b[2;1H\x1b[?25h") && !s.contains('a') && !s.contains('c'), "{s:?}");
        // A wide character whose second half is erased is drawn again whole.
        buf.set_string(0, 1, "日", Style::default());
        encode(&mut enc, &buf, None, &Links::new(), &mut Vec::new());
        enc.redraw(Rect::new(1, 1, 1, 1), &buf);
        let mut out = Vec::new();
        encode(&mut enc, &buf, None, &Links::new(), &mut out);
        assert!(text(&out).contains("\x1b[2;1H日"), "{:?}", text(&out));
    }

    #[test]
    fn colors_map_to_sgr() {
        let param = |c: Color, layer: Layer| {
            let mut out = Vec::new();
            color_param(&mut out, c, layer);
            text(&out)
        };
        assert_eq!(param(Color::LightBlue, Layer::Fg), "94");
        assert_eq!(param(Color::Green, Layer::Bg), "42");
        assert_eq!(param(Color::Indexed(200), Layer::Fg), "38;5;200");
        assert_eq!(param(Color::Rgb(1, 2, 3), Layer::Bg), "48;2;1;2;3");
        assert_eq!(param(Color::Cyan, Layer::Underline), "58;5;6");
        assert_eq!(param(Color::Reset, Layer::Underline), "59");
    }

    /// Every cell of a row but those from `x0` to `x1`, as kept runs.
    fn kept_but(area: Rect, (x0, x1, y): (u16, u16, u16)) -> Vec<Kept> {
        let mut kept = Vec::new();
        for row in area.top()..area.bottom() {
            if row != y {
                kept.push(Kept { y: row, x0: area.left(), x1: area.right() });
                continue;
            }
            if x0 > area.left() {
                kept.push(Kept { y: row, x0: area.left(), x1: x0 });
            }
            if x1 < area.right() {
                kept.push(Kept { y: row, x0: x1, x1: area.right() });
            }
        }
        kept
    }

    #[test]
    fn a_frame_after_one_cell_changed_sends_that_cell_alone() {
        let area = Rect::new(0, 0, 30, 6);
        let mut buf = Buffer::empty(area);
        for y in 0..6 {
            buf.set_string(0, y, "some text over the whole row", Style::default().fg(Color::Green));
        }
        let mut enc = Encoder::new();
        encode(&mut enc, &buf, None, &Links::new(), &mut Vec::new());
        buf[(7, 3)].set_char('X');
        let mut out = Vec::new();
        enc.encode_with_images(&buf, &kept_but(area, (7, 8, 3)), None, &Links::new(), &[], &[], &mut out);
        assert_eq!(text(&out), "\x1b[?2026h\x1b[?25l\x1b[4;8H\x1b[32mX\x1b[0m\x1b[?2026l");
        // Nothing changed: nothing is drawn.
        let mut out = Vec::new();
        enc.encode_with_images(&buf, &kept_but(area, (0, 0, 9)), None, &Links::new(), &[], &[], &mut out);
        assert_eq!(text(&out), "\x1b[?2026h\x1b[?25l\x1b[?2026l");
    }

    #[test]
    fn cells_to_draw_again_are_compared_even_when_kept() {
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "abcdef", Style::default());
        let mut enc = Encoder::new();
        encode(&mut enc, &buf, None, &Links::new(), &mut Vec::new());
        enc.redraw(Rect::new(2, 0, 2, 1), &buf);
        let mut out = Vec::new();
        enc.encode_with_images(&buf, &kept_but(area, (0, 0, 9)), None, &Links::new(), &[], &[], &mut out);
        assert!(text(&out).contains("\x1b[1;3Hcd"), "{:?}", text(&out));
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }

        /// A cell of a few kinds: narrow, blank, wide, an emoji with a variation selector, with
        /// combining marks, wider than two columns; plain, on a background, reversed, coloured.
        fn cell(&mut self, cell: &mut Cell) {
            const SYMBOLS: [&str; 8] = ["a", "b", " ", "", "日", "\u{2764}\u{fe0f}", "e\u{301}", "a\u{ff9e}\u{ff9e}"];
            let styles = [
                Style::default(),
                Style::default().bg(Color::Red),
                Style::default().add_modifier(Modifier::REVERSED),
                Style::default().fg(Color::Indexed(3)),
            ];
            cell.reset();
            cell.set_symbol(SYMBOLS[self.below(SYMBOLS.len() as u64) as usize]);
            cell.set_style(styles[self.below(styles.len() as u64) as usize]);
        }

        /// A frame of random cells and a few changed in the next one, with runs of cells equal
        /// in both said to be kept at random.
        fn frames(&mut self) -> (Buffer, Buffer, Vec<Kept>) {
            let area = Rect::new(0, 0, 2 + self.below(10) as u16, 1 + self.below(4) as u16);
            let mut prev = Buffer::empty(area);
            for i in 0..prev.content.len() {
                if self.below(3) > 0 {
                    self.cell(&mut prev.content[i]);
                }
            }
            // ratatui's diff reads past the end of a frame whose last cells were wide and are
            // made narrow: frames never end that way (the last column of a pane takes no wide
            // character).
            let len = prev.content.len();
            for cell in &mut prev.content[len.saturating_sub(2)..] {
                cell.set_symbol("z");
            }
            let mut next = prev.clone();
            for _ in 0..self.below(6) {
                let i = self.below(next.content.len() as u64) as usize;
                self.cell(&mut next.content[i]);
            }
            let mut kept = Vec::new();
            for y in area.top()..area.bottom() {
                let mut x = area.left();
                while x < area.right() {
                    let end = (x + 1 + self.below(4) as u16).min(area.right());
                    if (x..end).all(|cx| prev[(cx, y)] == next[(cx, y)]) && self.below(2) == 0 {
                        kept.push(Kept { y, x0: x, x1: end });
                    }
                    x = end;
                }
            }
            (prev, next, kept)
        }
    }

    /// Random frames and changes: the cells drawn are those ratatui's own diff finds, whether
    /// the unchanged cells are said to be kept or not.
    #[test]
    fn the_cells_drawn_are_those_ratatuis_diff_finds() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for case in 0..5000 {
            let (prev, next, kept) = rng.frames();
            let want = prev.diff(&next);
            let len = next.content.len();
            let mut whole = Vec::new();
            diff_ranges(Some(&prev.content), &next, &[(0, len)], &mut whole);
            assert_eq!(whole, want, "case {case}");
            let mut blank = Vec::new();
            diff_ranges(None, &next, &[(0, len)], &mut blank);
            assert_eq!(blank, Buffer::empty(next.area).diff(&next), "case {case}");
            let mut skipping = Vec::new();
            diff_ranges(Some(&prev.content), &next, &compared(&prev.content, &next, &kept), &mut skipping);
            assert_eq!(skipping, want, "case {case}: kept {kept:?}");
        }
    }

    /// The bytes sent do not depend on which unchanged cells are said to be kept, links and
    /// wide characters made narrow included.
    #[test]
    fn kept_cells_change_nothing_sent() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for case in 0..2000 {
            let (prev, next, kept) = rng.frames();
            let mut links = Links::new();
            if rng.below(2) == 0 {
                links.insert((0, 0), Arc::from("https://example.org"));
            }
            let (mut told, mut whole) = (Encoder::new(), Encoder::new());
            encode(&mut told, &prev, None, &Links::new(), &mut Vec::new());
            encode(&mut whole, &prev, None, &Links::new(), &mut Vec::new());
            let (mut a, mut b) = (Vec::new(), Vec::new());
            told.encode_with_images(&next, &kept, None, &links, &[], &[], &mut a);
            encode(&mut whole, &next, None, &links, &mut b);
            assert_eq!(text(&a), text(&b), "case {case}: kept {kept:?}");
        }
        // A wide character made narrow redraws the column it covered, which is kept.
        let area = Rect::new(0, 0, 8, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "ab日cdef", Style::default().bg(Color::Blue));
        let (mut told, mut whole) = (Encoder::new(), Encoder::new());
        encode(&mut told, &buf, None, &Links::new(), &mut Vec::new());
        encode(&mut whole, &buf, None, &Links::new(), &mut Vec::new());
        buf[(2, 0)].set_char('x');
        let (mut a, mut b) = (Vec::new(), Vec::new());
        told.encode_with_images(&buf, &[Kept { y: 0, x0: 3, x1: 8 }], None, &Links::new(), &[], &[], &mut a);
        encode(&mut whole, &buf, None, &Links::new(), &mut b);
        assert_eq!(text(&a), text(&b));
        assert!(text(&a).contains("\x1b[1;3H\x1b[44mx\x1b[49m "), "{:?}", text(&a));
    }
}
