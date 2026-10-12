//! The older part of a pane's history, kept compact once it left the emulator: each line's text
//! and its runs of attributes, without the blank cells that make every row of the emulator's grid
//! as wide as the pane. Lines are laid out into rows again, at the pane's width then, when the
//! view, the copy-mode cursor or a search goes back to them (see `archive.rs`).

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use alacritty_terminal::grid::Row;
use alacritty_terminal::index::Column;
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use unicode_width::UnicodeWidthChar;

/// Bytes of lines per block: the oldest lines go a block at a time.
const BLOCK_BYTES: usize = 16 << 10;

/// The flags that say where a character lies on the grid rather than how it looks.
const LAYOUT: Flags =
    Flags::WRAPLINE.union(Flags::WIDE_CHAR).union(Flags::WIDE_CHAR_SPACER).union(Flags::LEADING_WIDE_CHAR_SPACER);
const SPACERS: Flags = Flags::WIDE_CHAR_SPACER.union(Flags::LEADING_WIDE_CHAR_SPACER);

// What a line holds besides its text.
/// Attributes other than the defaults.
const RUNS: u8 = 1;
/// The blank end of its last row has a colour.
const FILL: u8 = 2;
/// The line goes on in the next one: it was cut, too long to keep whole.
const WRAPS: u8 = 4;
/// The width of each character is written down: the grid did not follow the usual rules.
const WIDTHS: u8 = 8;
/// Image cells.
const IMAGE: u8 = 16;
/// Only ASCII characters, each in a column of its own.
const ASCII: u8 = 32;

// Which attributes a run changes from the run before.
const FG: u8 = 1;
const BG: u8 = 2;
const STYLE: u8 = 4;
const UNDERLINE: u8 = 8;
const LINK: u8 = 16;

/// The named colours, by the number they are kept as.
const NAMED: [NamedColor; 29] = [
    NamedColor::Black,
    NamedColor::Red,
    NamedColor::Green,
    NamedColor::Yellow,
    NamedColor::Blue,
    NamedColor::Magenta,
    NamedColor::Cyan,
    NamedColor::White,
    NamedColor::BrightBlack,
    NamedColor::BrightRed,
    NamedColor::BrightGreen,
    NamedColor::BrightYellow,
    NamedColor::BrightBlue,
    NamedColor::BrightMagenta,
    NamedColor::BrightCyan,
    NamedColor::BrightWhite,
    NamedColor::Foreground,
    NamedColor::Background,
    NamedColor::Cursor,
    NamedColor::DimBlack,
    NamedColor::DimRed,
    NamedColor::DimGreen,
    NamedColor::DimYellow,
    NamedColor::DimBlue,
    NamedColor::DimMagenta,
    NamedColor::DimCyan,
    NamedColor::DimWhite,
    NamedColor::BrightForeground,
    NamedColor::DimForeground,
];
const INDEXED: u8 = 0xfd;
const SPEC: u8 = 0xfe;
const NONE: u8 = 0xff;

/// Lines, oldest first. Each is kept as: its text's length, its flags, its text, then as its
/// flags say the widths of its characters, its runs of attributes (each with its length first)
/// and the colour its last row ends with.
#[derive(Default)]
pub(super) struct History {
    blocks: VecDeque<Block>,
    lines: usize,
    /// The pane's width, and the rows the lines take at `counted` columns: counted again when
    /// needed after the width changed, not at each resize.
    width: usize,
    rows: usize,
    counted: usize,
    /// The images whose cells are here, with the number of lines holding them.
    images: HashMap<u32, usize>,
    // Reused while encoding.
    text: Vec<u8>,
    widths: Vec<u8>,
    runs: Vec<u8>,
}

struct Block {
    data: Vec<u8>,
    /// Where each line starts in `data`; the first `first` lines are gone.
    starts: Vec<u32>,
    first: usize,
}

impl Block {
    fn new(size: usize) -> Block {
        Block { data: Vec::with_capacity(size.max(BLOCK_BYTES)), starts: Vec::new(), first: 0 }
    }

    fn len(&self) -> usize {
        self.starts.len() - self.first
    }

    fn line(&self, i: usize) -> Line<'_> {
        let start = self.starts[i] as usize;
        let end = self.starts.get(i + 1).map_or(self.data.len(), |e| *e as usize);
        Line::parse(&self.data[start..end])
    }
}

impl History {
    pub(super) fn len(&self) -> usize {
        self.lines
    }

    pub(super) fn is_empty(&self) -> bool {
        self.lines == 0
    }

    /// Rows the lines take at the pane's width.
    pub(super) fn rows(&mut self) -> usize {
        if self.counted != self.width {
            let width = self.width;
            self.rows = self.oldest_first().map(|l| l.rows_at(width)).sum();
            self.counted = width;
        }
        self.rows
    }

    /// The pane is `width` columns wide now.
    pub(super) fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    /// Memory held, in bytes.
    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.blocks.iter().map(|b| b.data.capacity() + 4 * b.starts.capacity()).sum()
    }

    pub(super) fn clear(&mut self) {
        self.blocks.clear();
        self.lines = 0;
        self.rows = 0;
        self.counted = self.width;
        self.images.clear();
    }

    /// The images that have cells here.
    pub(super) fn images(&self) -> impl Iterator<Item = u32> + '_ {
        self.images.keys().copied()
    }

    /// Add the newest line: the emulator's `rows`, each but the last wrapped onto the next. With
    /// `wraps`, the last one is too: the line goes on in the next one added. The cells of the last
    /// row from `extent` on are all alike (it may be its length).
    pub(super) fn push(&mut self, rows: &[&[Cell]], wraps: bool, extent: usize) {
        let Some(last) = rows.last() else { return };
        self.width = last.len();
        let (mut end, fill) = if wraps { (last.len(), None) } else { trim(last, extent) };
        if end == 0 && rows.len() > 1 {
            // A row the line wrapped onto, blank since: a cell of it keeps it a row of the line.
            end = 1;
        }
        let mut flags = 0;
        let (glyphs, runs) = match self.encode(rows, end, false) {
            Some(counts) => counts,
            None => {
                flags |= WIDTHS;
                self.encode(rows, end, true).unwrap_or_default()
            }
        };
        if runs > 0 {
            flags |= RUNS;
        }
        if fill.is_some() {
            flags |= FILL;
        }
        if wraps {
            flags |= WRAPS;
        }
        if flags & WIDTHS == 0 && self.text.is_ascii() {
            flags |= ASCII;
        } else if has_placeholder(&self.text) {
            flags |= IMAGE;
            let ids = rows.iter().flat_map(|r| r.iter()).filter_map(crate::graphics::image_of_cell).collect();
            for id in dedup(ids) {
                *self.images.entry(id).or_default() += 1;
            }
        }
        let size = self.text.len() + self.widths.len() + self.runs.len() + 32;
        if self.blocks.back().is_none_or(|b| b.data.len() + size > b.data.capacity() && b.len() > 0) {
            self.blocks.push_back(Block::new(size));
        }
        let block = self.blocks.back_mut().expect("a block was just added");
        block.starts.push(block.data.len() as u32);
        let out = &mut block.data;
        put_varint(out, self.text.len());
        out.push(flags);
        out.extend_from_slice(&self.text);
        for (flag, count, section) in [(WIDTHS, glyphs, &self.widths), (RUNS, runs, &self.runs)] {
            if flags & flag != 0 {
                let mut head = [0; 10];
                let n = varint_into(&mut head, count);
                put_varint(out, n + section.len());
                out.extend_from_slice(&head[..n]);
                out.extend_from_slice(section);
            }
        }
        if let Some(bg) = fill {
            put_color(out, bg);
        }
        if self.lines == 0 {
            (self.rows, self.counted) = (0, self.width);
        }
        #[cfg(test)]
        {
            // Laid out again at the same width, the line takes the rows it took.
            let block = self.blocks.back().expect("just added");
            assert_eq!(block.line(block.starts.len() - 1).rows_at(self.width), rows.len());
        }
        self.lines += 1;
        if self.counted == self.width {
            self.rows += rows.len();
        }
    }

    /// Write the text, the widths when `explicit`, and the runs of a line into the scratch
    /// buffers; returns the number of characters and of runs (none when all look plain). None
    /// when the line needs its widths written down (only checked without them).
    fn encode(&mut self, rows: &[&[Cell]], last_end: usize, explicit: bool) -> Option<(usize, usize)> {
        self.text.clear();
        self.widths.clear();
        self.runs.clear();
        let (text, widths, out) = (&mut self.text, &mut self.widths, &mut self.runs);
        let mut glyphs = 0usize;
        let mut runs = Runs::default();
        // The run being counted: its first cell, and its length.
        let mut first: Option<&Cell> = None;
        let mut n = 0;
        for (i, row) in rows.iter().enumerate() {
            let end = if i + 1 == rows.len() { last_end } else { row.len() };
            let cells = &row[..end];
            text.reserve(cells.len());
            for (col, cell) in cells.iter().enumerate() {
                let c = cell.c;
                if cell.extra.is_none() && !cell.flags.intersects(LAYOUT) && (' '..'\x7f').contains(&c) {
                    text.push(c as u8);
                    if explicit {
                        widths.push(0);
                    }
                } else {
                    if cell.flags.intersects(SPACERS | Flags::WIDE_CHAR) && !explicit && !laid_out(rows, i, col) {
                        return None;
                    }
                    if cell.flags.intersects(SPACERS) && !explicit {
                        continue;
                    }
                    push_char(text, c);
                    let zerowidth = cell.zerowidth().unwrap_or_default();
                    for z in zerowidth {
                        push_char(text, *z);
                    }
                    if explicit {
                        put_varint(widths, zerowidth.len() << 3 | usize::from(layout_bits(cell.flags)));
                    } else if !follows_rules(c, cell.flags.contains(Flags::WIDE_CHAR), zerowidth) {
                        return None;
                    }
                }
                glyphs += 1;
                match first {
                    Some(f) if same_look(f, cell) => n += 1,
                    _ => {
                        if let Some(f) = first {
                            runs.add(n, f, out);
                        }
                        (first, n) = (Some(cell), 1);
                    }
                }
            }
        }
        if let Some(f) = first {
            runs.add(n, f, out);
        }
        if runs.count == 1 && !runs.styled {
            self.runs.clear();
            runs.count = 0;
        }
        Some((glyphs, runs.count))
    }

    /// Drop the oldest lines while they take more than `rows` rows. Until they are counted at the
    /// pane's width, only while there are more lines than that (each takes a row at least); `exact`
    /// counts them.
    pub(super) fn trim(&mut self, rows: usize, exact: bool) {
        if exact {
            self.rows();
        }
        while (if self.counted == self.width { self.rows } else { self.lines }) > rows && !self.is_empty() {
            self.drop_oldest();
        }
    }

    fn drop_oldest(&mut self) {
        let Some(block) = self.blocks.front_mut() else { return };
        let line = block.line(block.first);
        let rows = if self.counted == self.width { line.rows_at(self.width) } else { 0 };
        if line.flags & IMAGE != 0 {
            forget_images(&mut self.images, &line);
        }
        block.first += 1;
        if block.len() == 0 {
            self.blocks.pop_front();
        }
        self.lines -= 1;
        self.rows -= rows;
    }

    /// The newest line.
    #[cfg(test)]
    pub(super) fn newest(&self) -> Option<Line<'_>> {
        let block = self.blocks.back()?;
        (block.len() > 0).then(|| block.line(block.starts.len() - 1))
    }

    pub(super) fn drop_newest(&mut self) {
        let Some(block) = self.blocks.back_mut() else { return };
        let Some(start) = block.starts.pop() else { return };
        let line = Line::parse(&block.data[start as usize..]);
        let rows = if self.counted == self.width { line.rows_at(self.width) } else { 0 };
        if line.flags & IMAGE != 0 {
            forget_images(&mut self.images, &line);
        }
        block.data.truncate(start as usize);
        if block.len() == 0 {
            self.blocks.pop_back();
        }
        self.lines -= 1;
        self.rows -= rows;
    }

    /// The lines, newest first.
    pub(super) fn newest_first(&self) -> impl Iterator<Item = Line<'_>> {
        self.blocks.iter().rev().flat_map(|b| (b.first..b.starts.len()).rev().map(move |i| b.line(i)))
    }

    /// The lines, oldest first.
    pub(super) fn oldest_first(&self) -> impl Iterator<Item = Line<'_>> {
        self.blocks.iter().flat_map(|b| (b.first..b.starts.len()).map(move |i| b.line(i)))
    }
}

fn forget_images(images: &mut HashMap<u32, usize>, line: &Line) {
    let ids = line.unwrapped()[..].iter().filter_map(crate::graphics::image_of_cell).collect();
    for id in dedup(ids) {
        if let Some(n) = images.get_mut(&id) {
            *n -= 1;
            if *n == 0 {
                images.remove(&id);
            }
        }
    }
}

fn dedup(mut ids: Vec<u32>) -> Vec<u32> {
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Whether UTF-8 text holds an image cell's character.
fn has_placeholder(text: &[u8]) -> bool {
    let mut buf = [0; 4];
    let needle = crate::graphics::PLACEHOLDER.encode_utf8(&mut buf).as_bytes();
    // Its first byte (F4) is rare: only characters past U+FFFFF start with it.
    text.iter().enumerate().any(|(i, b)| *b == needle[0] && text[i..].starts_with(needle))
}

/// Whether the cells of a wide character are where the emulator puts them, for cell `col` of
/// `rows[i]`: the character, then a spacer; or a spacer in the last column of a row the character
/// did not fit on, which goes on with it on the next row.
fn laid_out(rows: &[&[Cell]], i: usize, col: usize) -> bool {
    let row = rows[i];
    let flags = row[col].flags;
    if flags.contains(Flags::WIDE_CHAR) {
        !flags.intersects(SPACERS) && row.get(col + 1).is_some_and(|c| c.flags.contains(Flags::WIDE_CHAR_SPACER))
    } else if flags.contains(Flags::WIDE_CHAR_SPACER) {
        !flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) && col > 0 && row[col - 1].flags.contains(Flags::WIDE_CHAR)
    } else {
        col + 1 == row.len()
            && flags.contains(Flags::WRAPLINE)
            && rows.get(i + 1).is_some_and(|next| next[0].flags.contains(Flags::WIDE_CHAR))
    }
}

/// The flags that place a cell of a wide character, as kept for a line whose cells are kept as
/// they are.
fn layout_bits(flags: Flags) -> u8 {
    u8::from(flags.contains(Flags::WIDE_CHAR))
        | u8::from(flags.contains(Flags::WIDE_CHAR_SPACER)) << 1
        | u8::from(flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)) << 2
}

fn layout_flags(bits: usize) -> Flags {
    let mut flags = Flags::empty();
    flags.set(Flags::WIDE_CHAR, bits & 1 != 0);
    flags.set(Flags::WIDE_CHAR_SPACER, bits & 2 != 0);
    flags.set(Flags::LEADING_WIDE_CHAR_SPACER, bits & 4 != 0);
    flags
}

/// Whether a cell holds what the emulator makes of its text: a character of one or two columns,
/// as its width says, followed by characters of none.
fn follows_rules(c: char, wide: bool, zerowidth: &[char]) -> bool {
    let width = c.width();
    width != Some(0) && (width == Some(2)) == wide && zerowidth.iter().all(|z| z.width() == Some(0))
}

/// Where the last row of a line ends: past its last cell unlike the blank cells that fill its
/// end, and their background when it is not the default. Its cells from `extent` on are all
/// alike: only those before are read.
fn trim(row: &[Cell], extent: usize) -> (usize, Option<Color>) {
    let Some(last) = row.last() else { return (0, None) };
    let default_fg = Color::Named(NamedColor::Foreground);
    if last.c != ' ' || !last.flags.is_empty() || last.extra.is_some() || last.fg != default_fg {
        return (row.len(), None);
    }
    let bg = last.bg;
    let end = row[..extent.min(row.len())]
        .iter()
        .rposition(|c| c.c != ' ' || c.bg != bg || c.fg != default_fg || !c.flags.is_empty() || c.extra.is_some())
        .map_or(0, |i| i + 1);
    (end, (bg != Color::Named(NamedColor::Background)).then_some(bg))
}

/// Whether two cells look the same, characters aside.
#[inline]
fn same_look(a: &Cell, b: &Cell) -> bool {
    (a.flags.bits() ^ b.flags.bits()) & !LAYOUT.bits() == 0
        && a.fg == b.fg
        && a.bg == b.bg
        && match (&a.extra, &b.extra) {
            (None, None) => true,
            (Some(x), Some(y)) if Arc::ptr_eq(x, y) => true,
            _ => a.underline_color() == b.underline_color() && a.hyperlink() == b.hyperlink(),
        }
}

/// How a run of characters looks.
#[derive(Clone, PartialEq)]
struct Look {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
    link: Option<Hyperlink>,
}

impl Default for Look {
    fn default() -> Look {
        let cell = Cell::default();
        Look { fg: cell.fg, bg: cell.bg, flags: Flags::empty(), underline: None, link: None }
    }
}

impl Look {
    fn of(cell: &Cell) -> Look {
        Look {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags.difference(LAYOUT),
            underline: cell.underline_color(),
            link: cell.hyperlink(),
        }
    }

    /// A blank cell that looks like this.
    fn cell(&self) -> Cell {
        let mut cell = Cell { fg: self.fg, bg: self.bg, flags: self.flags, ..Cell::default() };
        if self.underline.is_some() {
            cell.set_underline_color(self.underline);
        }
        if self.link.is_some() {
            cell.set_hyperlink(self.link.clone());
        }
        cell
    }
}

/// Runs being written: each is its number of characters, then what changes from the one before.
#[derive(Default)]
struct Runs {
    count: usize,
    styled: bool,
    last: Look,
}

impl Runs {
    /// Write a run of `n` characters that look like `cell`.
    fn add(&mut self, n: usize, cell: &Cell, out: &mut Vec<u8>) {
        put_varint(out, n);
        let look = if cell.extra.is_none() {
            Look { fg: cell.fg, bg: cell.bg, flags: cell.flags.difference(LAYOUT), underline: None, link: None }
        } else {
            Look::of(cell)
        };
        let prev = &self.last;
        let mut mask = 0;
        if look.fg != prev.fg {
            mask |= FG;
        }
        if look.bg != prev.bg {
            mask |= BG;
        }
        if look.flags != prev.flags {
            mask |= STYLE;
        }
        if look.underline != prev.underline {
            mask |= UNDERLINE;
        }
        if look.link != prev.link {
            mask |= LINK;
        }
        out.push(mask);
        if mask & FG != 0 {
            put_color(out, look.fg);
        }
        if mask & BG != 0 {
            put_color(out, look.bg);
        }
        if mask & STYLE != 0 {
            out.extend_from_slice(&look.flags.bits().to_le_bytes());
        }
        if mask & UNDERLINE != 0 {
            match look.underline {
                Some(c) => put_color(out, c),
                None => out.push(NONE),
            }
        }
        if mask & LINK != 0 {
            match &look.link {
                Some(link) => {
                    put_varint(out, link.id().len() + 1);
                    out.extend_from_slice(link.id().as_bytes());
                    put_varint(out, link.uri().len());
                    out.extend_from_slice(link.uri().as_bytes());
                }
                None => put_varint(out, 0),
            }
        }
        // The first run changes nothing from the defaults when its characters look plain.
        self.styled |= mask != 0;
        self.count += 1;
        self.last = look;
    }
}

/// One line, as kept.
pub(super) struct Line<'a> {
    flags: u8,
    /// UTF-8, read as such only when its characters are needed.
    bytes: &'a [u8],
    widths: &'a [u8],
    runs: &'a [u8],
    fill: Option<Color>,
}

impl<'a> Line<'a> {
    fn parse(bytes: &'a [u8]) -> Line<'a> {
        let mut r = Reader { bytes, at: 0 };
        let len = r.varint();
        let flags = r.byte();
        let text = r.take(len);
        let mut section = |flag: u8| {
            if flags & flag == 0 {
                return &[][..];
            }
            let len = r.varint();
            r.take(len)
        };
        let widths = section(WIDTHS);
        let runs = section(RUNS);
        let fill = (flags & FILL != 0).then(|| r.color());
        Line { flags, bytes: text, widths, runs, fill }
    }

    /// The text: each character followed by those drawn over it.
    pub(super) fn text(&self) -> &'a str {
        // Written from a str, kept as written.
        std::str::from_utf8(self.bytes).unwrap_or_default()
    }

    /// The text a search reads: the characters without those drawn over them, as the emulator's
    /// search reads its cells.
    pub(super) fn searched(&self) -> Cow<'a, str> {
        if self.flags & ASCII != 0 {
            Cow::Borrowed(self.text())
        } else {
            Cow::Owned(self.glyphs().filter(|g| !g.flags.intersects(SPACERS)).map(|g| g.base).collect())
        }
    }

    /// Columns its characters take, on one row.
    pub(super) fn columns(&self) -> usize {
        self.glyphs().map(|g| if g.wide { 2 } else { 1 }).sum()
    }

    /// The line on one row, as wide as it needs: its characters, then one blank cell (in its
    /// colour, when its end has one).
    pub(super) fn unwrapped(&self) -> Row<Cell> {
        let mut rows = Vec::with_capacity(1);
        self.decode(self.columns() + 1, &mut rows);
        rows.pop().unwrap_or_else(|| Row::new(1))
    }

    fn glyphs(&self) -> Glyphs<'a> {
        let mut widths = Reader { bytes: self.widths, at: 0 };
        // The number of characters first.
        widths.varint();
        Glyphs { text: self.text(), at: 0, widths: (self.flags & WIDTHS != 0).then_some(widths) }
    }

    /// Lay the line out at `width` columns: `place` gets each character with the column it
    /// starts at, or None at the end of each row but the last.
    fn layout(&self, width: usize, mut place: impl FnMut(Option<(&Glyph<'a>, usize)>)) {
        let mut col = 0;
        for g in self.glyphs() {
            let w = if g.wide && width >= 2 { 2 } else { 1 };
            if col + w > width {
                place(None);
                col = 0;
            }
            place(Some((&g, col)));
            col += w;
        }
    }

    /// Rows it takes at `width` columns.
    pub(super) fn rows_at(&self, width: usize) -> usize {
        if self.flags & ASCII != 0 {
            return self.bytes.len().div_ceil(width.max(1)).max(1);
        }
        let mut rows = 1;
        self.layout(width, |g| rows += usize::from(g.is_none()));
        rows
    }

    /// Add the rows of the grid this line makes at `width` columns to `out`; returns how many.
    pub(super) fn decode(&self, width: usize, out: &mut Vec<Row<Cell>>) -> usize {
        let start = out.len();
        out.extend((0..self.rows_at(width)).map(|_| Row::new(width.max(1))));
        self.decode_into(width, &mut out[start..], true)
    }

    /// Write the rows of the grid this line makes at `width` columns into `rows`, as many as it
    /// makes ([`Line::rows_at`]); returns how many. With `blank`, they hold default cells already
    /// and only the others are written.
    pub(super) fn decode_into(&self, width: usize, rows: &mut [Row<Cell>], blank: bool) -> usize {
        let width = width.max(1);
        let mut looks = Reader { bytes: self.runs, at: 0 };
        let runs = looks.varint();
        let mut look = Look::default();
        let mut template = Cell::default();
        let (mut run, mut left) = (0, usize::MAX);
        if runs > 0 {
            left = looks.varint();
            looks.look(&mut look);
            template = look.cell();
            run = 1;
        }
        let (mut r, mut col) = (0, 0);
        for g in self.glyphs() {
            while left == 0 {
                if run < runs {
                    left = looks.varint();
                    looks.look(&mut look);
                    template = look.cell();
                    run += 1;
                } else {
                    template = Cell::default();
                    left = usize::MAX;
                }
            }
            left -= 1;
            let w = if g.wide && width >= 2 { 2 } else { 1 };
            if col + w > width {
                let Some(row) = rows.get_mut(r) else { return r };
                if col < width {
                    let mut spacer = template.clone();
                    spacer.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
                    row[Column(col)] = spacer;
                }
                row[Column(width - 1)].flags.insert(Flags::WRAPLINE);
                (r, col) = (r + 1, 0);
            }
            let Some(row) = rows.get_mut(r) else { return r };
            let mut cell = template.clone();
            cell.c = g.base;
            cell.flags.insert(g.flags);
            for z in g.zerowidth.chars() {
                cell.push_zerowidth(z);
            }
            if w == 2 {
                cell.flags.insert(Flags::WIDE_CHAR);
                let mut spacer = template.clone();
                spacer.flags.insert(Flags::WIDE_CHAR_SPACER);
                row[Column(col + 1)] = spacer;
            }
            row[Column(col)] = cell;
            col += w;
        }
        let Some(row) = rows.get_mut(r) else { return r };
        if let Some(bg) = self.fill {
            row[Column(col)..].fill(Cell::from(bg));
        } else if !blank {
            row[Column(col)..].fill(Cell::default());
        }
        if self.flags & WRAPS != 0 {
            row[Column(width - 1)].flags.insert(Flags::WRAPLINE);
        }
        r + 1
    }

    /// Add the text of each row this line makes at `width` columns to `out`, as the text of a
    /// grid row reads: image cells blank, the blanks at its end left out.
    pub(super) fn row_texts(&self, width: usize, out: &mut Vec<String>) {
        let mut text = String::new();
        self.layout(width, |g| match g {
            Some((g, _)) if g.flags.contains(Flags::WIDE_CHAR_SPACER) => {}
            Some((g, _)) if g.base == crate::graphics::PLACEHOLDER => text.push(' '),
            Some((g, _)) => {
                text.push(g.base);
                text.push_str(g.zerowidth);
            }
            None => {
                out.push(text.trim_end().to_string());
                text.clear();
            }
        });
        out.push(text.trim_end().to_string());
    }
}

/// A character and those drawn over it: two columns when `wide`, else one, with `flags` when
/// the cells of its line are kept as they are.
struct Glyph<'a> {
    base: char,
    zerowidth: &'a str,
    wide: bool,
    flags: Flags,
}

struct Glyphs<'a> {
    text: &'a str,
    at: usize,
    /// The widths written down, when they are.
    widths: Option<Reader<'a>>,
}

impl<'a> Iterator for Glyphs<'a> {
    type Item = Glyph<'a>;

    fn next(&mut self) -> Option<Glyph<'a>> {
        let rest = &self.text[self.at..];
        let base = rest.chars().next()?;
        let mut end = base.len_utf8();
        let mut wide = false;
        let mut flags = Flags::empty();
        match &mut self.widths {
            Some(w) => {
                let n = w.varint();
                flags = layout_flags(n & 7);
                for c in rest[end..].chars().take(n >> 3) {
                    end += c.len_utf8();
                }
            }
            None => {
                wide = base.width() == Some(2);
                // An ASCII character is never drawn over another.
                if rest.as_bytes().get(end).is_some_and(|b| !b.is_ascii()) {
                    for c in rest[end..].chars() {
                        if c.width() != Some(0) {
                            break;
                        }
                        end += c.len_utf8();
                    }
                }
            }
        }
        let zerowidth = &rest[base.len_utf8()..end];
        self.at += end;
        Some(Glyph { base, zerowidth, wide, flags })
    }
}

/// Reads what [`History::push`] wrote. Past the end it reads zeros: nothing is ever cut short,
/// but a mistake here must not bring the server down.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> u8 {
        let b = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }

    fn take(&mut self, n: usize) -> &'a [u8] {
        let start = self.at.min(self.bytes.len());
        self.at = self.at.saturating_add(n).min(self.bytes.len());
        &self.bytes[start..self.at]
    }

    fn varint(&mut self) -> usize {
        let mut v = 0usize;
        for shift in (0..64).step_by(7) {
            let b = self.byte();
            v |= usize::from(b & 0x7f) << shift;
            if b < 0x80 {
                break;
            }
        }
        v
    }

    fn color(&mut self) -> Color {
        match self.byte() {
            INDEXED => Color::Indexed(self.byte()),
            SPEC => Color::Spec(Rgb { r: self.byte(), g: self.byte(), b: self.byte() }),
            n => Color::Named(NAMED.get(usize::from(n)).copied().unwrap_or(NamedColor::Foreground)),
        }
    }

    /// Apply the changes of a run to `look`.
    fn look(&mut self, look: &mut Look) {
        let mask = self.byte();
        if mask & FG != 0 {
            look.fg = self.color();
        }
        if mask & BG != 0 {
            look.bg = self.color();
        }
        if mask & STYLE != 0 {
            let bits = u16::from_le_bytes([self.byte(), self.byte()]);
            look.flags = Flags::from_bits_truncate(bits).difference(LAYOUT);
        }
        if mask & UNDERLINE != 0 {
            look.underline = if self.bytes.get(self.at) == Some(&NONE) {
                self.at += 1;
                None
            } else {
                Some(self.color())
            };
        }
        if mask & LINK != 0 {
            look.link = match self.varint() {
                0 => None,
                n => {
                    let id = String::from_utf8_lossy(self.take(n - 1)).into_owned();
                    let len = self.varint();
                    let uri = String::from_utf8_lossy(self.take(len)).into_owned();
                    Some(Hyperlink::new(Some(id), uri))
                }
            };
        }
    }
}

fn put_varint(out: &mut Vec<u8>, v: usize) {
    let mut buf = [0; 10];
    let n = varint_into(&mut buf, v);
    out.extend_from_slice(&buf[..n]);
}

/// Write `v` as a varint at the start of `buf`; returns its length.
fn varint_into(buf: &mut [u8; 10], mut v: usize) -> usize {
    let mut n = 0;
    while v >= 0x80 {
        buf[n] = v as u8 | 0x80;
        v >>= 7;
        n += 1;
    }
    buf[n] = v as u8;
    n + 1
}

fn put_color(out: &mut Vec<u8>, c: Color) {
    match c {
        // The sixteen colours, then the others from Foreground (256) on, as in `NAMED`.
        Color::Named(n) => out.push(match n as usize {
            i @ 0..16 => i as u8,
            i @ 256..269 => (i - 240) as u8,
            _ => 16,
        }),
        Color::Indexed(i) => out.extend_from_slice(&[INDEXED, i]),
        Color::Spec(Rgb { r, g, b }) => out.extend_from_slice(&[SPEC, r, g, b]),
    }
}

fn push_char(out: &mut Vec<u8>, c: char) {
    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line as GridLine;
    use alacritty_terminal::term::{Config as TermConfig, Term};
    use alacritty_terminal::vte::ansi::Processor;

    use super::super::pty::{Listener, Size};
    use super::*;

    fn term(cols: u16, rows: u16, history: usize, bytes: &[u8]) -> Term<Listener> {
        let config = TermConfig { scrolling_history: history, ..TermConfig::default() };
        let mut t = Term::new(config, &Size { cols, rows }, Listener::default());
        let mut parser: Processor = Processor::new();
        parser.advance(&mut t, bytes);
        t
    }

    /// The rows of the grid, history first, grouped into the lines they make.
    fn logical(t: &Term<Listener>) -> Vec<Vec<Row<Cell>>> {
        let grid = t.grid();
        let cols = grid.columns();
        let mut lines = Vec::new();
        let mut current = Vec::new();
        for l in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
            let row = grid[GridLine(l)].clone();
            let wraps = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            current.push(row);
            if !wraps {
                lines.push(std::mem::take(&mut current));
            }
        }
        lines
    }

    fn store(t: &Term<Listener>) -> History {
        let mut h = History::default();
        for rows in logical(t) {
            let rows: Vec<&[Cell]> = rows.iter().map(|r| &r[..]).collect();
            h.push(&rows, false, usize::MAX);
        }
        h
    }

    const SAMPLE: &str = "\x1b[1;31mred\x1b[0m plain \x1b[38;2;1;2;3mrgb\x1b[0m \x1b[4:3;58;5;9mcurl\x1b[0m\r\n\
        日本語の文字列はここで折り返されます end\r\n\
        \x1b]8;id=7;https://example.org\x1b\\link\x1b]8;;\x1b\\ after\r\n\
        \x1b[44mbar\x1b[K\x1b[0m\r\n\
        e\u{301}t\u{e9}\ttab\r\n\
        \x1b[7minverse\x1b[0m   \x1b[2;3mdim italic\x1b[0m\r\n\
        abcdefghijklmnopqrstuvwxyz0123456789 wrapped over rows\r\nlast";

    #[test]
    fn rows_come_back_as_they_were_at_the_same_width() {
        for cols in [7, 13, 40] {
            let t = term(cols, 30, 100, SAMPLE.as_bytes());
            let h = store(&t);
            let original = logical(&t);
            assert_eq!(h.len(), original.len());
            for (line, rows) in h.oldest_first().zip(&original) {
                let mut back = Vec::new();
                assert_eq!(line.decode(cols as usize, &mut back), rows.len());
                assert_eq!(&back, rows, "width {cols}: {:?}", line.text());
                assert_eq!(line.rows_at(cols as usize), rows.len());
            }
        }
    }

    #[test]
    fn lines_are_laid_out_again_at_another_width() {
        let wide = term(60, 30, 100, SAMPLE.as_bytes());
        let narrow = term(11, 40, 100, SAMPLE.as_bytes());
        let h = store(&wide);
        let expected: Vec<Row<Cell>> = logical(&narrow).into_iter().flatten().collect();
        let mut back = Vec::new();
        for line in h.oldest_first() {
            line.decode(11, &mut back);
        }
        // The emulator's own blank rows at the end are lines of their own.
        assert_eq!(back[..], expected[..back.len()]);
        let mut texts = Vec::new();
        for line in h.oldest_first() {
            line.row_texts(11, &mut texts);
        }
        let top = narrow.grid().history_size() as i32;
        let grid: Vec<String> =
            (0..back.len() as i32).map(|l| super::super::screen::row_text(&narrow, GridLine(l - top))).collect();
        assert_eq!(texts, grid);
    }

    #[test]
    fn a_line_is_kept_in_a_few_bytes() {
        let t = term(200, 3, 0, b"\x1b[32mINFO\x1b[0m request served in 12 ms");
        let mut h = History::default();
        h.push(&[&t.grid()[GridLine(0)][..]], false, usize::MAX);
        let line = h.newest().unwrap();
        assert_eq!(line.text(), "INFO request served in 12 ms");
        // Header, text and two runs, against 200 cells of 24 bytes.
        let size = h.blocks[0].data.len();
        assert!(size < 50, "{size} bytes");
    }

    #[test]
    fn the_oldest_lines_go_first_and_the_newest_come_back() {
        let mut h = History::default();
        let t = term(10, 1, 0, b"x");
        let row = &t.grid()[GridLine(0)][..];
        for _ in 0..20_000 {
            h.push(&[row], false, usize::MAX);
        }
        assert_eq!((h.len(), h.rows()), (20_000, 20_000));
        assert!(h.blocks.len() > 1);
        h.trim(1000, true);
        assert_eq!((h.len(), h.rows()), (1000, 1000));
        for _ in 0..1000 {
            assert_eq!(h.newest().unwrap().text(), "x");
            h.drop_newest();
        }
        assert!(h.is_empty() && h.blocks.is_empty());
    }

    /// What adding a line costs: `cargo test --release -- --ignored --nocapture push_cost`.
    #[test]
    #[ignore]
    fn push_cost() {
        // As the compare harness writes its coloured file: a colour every word.
        let mut colourful = String::new();
        for i in 0..50u32 {
            colourful.push_str(&match i % 5 {
                0 => format!("\x1b[38;5;{}m", i * 5),
                1 => format!("\x1b[38;2;{};{};{}m", i, i * 2, i * 3),
                2 => format!("\x1b[1;3{}m", i % 8),
                3 => "\x1b[0m".to_string(),
                _ => format!("\x1b[48;5;{}m", 232 + i % 24),
            });
            colourful.push_str(["error ", "Compiling ", "fn ", "αβγ ", "Ok(()) "][i as usize % 5]);
        }
        let lines: [&[u8]; 3] =
            [b"1234567", b"\x1b[2m00:00:01.037\x1b[0m \x1b[32mINFO\x1b[0m  p00 req=000001 ok", colourful.as_bytes()];
        for bytes in lines {
            let t = term(169, 4, 0, bytes);
            let wrapped = t.grid()[GridLine(0)][alacritty_terminal::index::Column(168)].flags.contains(Flags::WRAPLINE);
            let rows: Vec<&[Cell]> = (0..if wrapped { 3 } else { 1 }).map(|l| &t.grid()[GridLine(l)][..]).collect();
            let extent = rows.last().unwrap().iter().rposition(|c| c.c != ' ').map_or(0, |i| i + 2);
            let mut h = History::default();
            let start = std::time::Instant::now();
            for _ in 0..200_000 {
                h.push(&rows, false, extent);
                if h.len() > 10_000 {
                    h.trim(5_000, false);
                }
            }
            eprintln!("{} rows: {:?} per line", rows.len(), start.elapsed() / 200_000);
        }
    }

    #[test]
    fn named_colours_are_all_kept() {
        for n in NAMED {
            let mut out = Vec::new();
            put_color(&mut out, Color::Named(n));
            assert_eq!(Reader { bytes: &out, at: 0 }.color(), Color::Named(n));
        }
    }

    #[test]
    fn cells_that_do_not_follow_the_rules_keep_their_widths() {
        let mut odd = Cell { c: 'x', ..Cell::default() };
        odd.flags.insert(Flags::WIDE_CHAR);
        odd.push_zerowidth('y');
        let mut spacer = Cell::default();
        spacer.flags.insert(Flags::WIDE_CHAR_SPACER);
        let mut row = vec![Cell { c: 'a', ..Cell::default() }, odd, spacer, Cell { c: 'b', ..Cell::default() }];
        row.resize(6, Cell::default());
        let mut h = History::default();
        h.push(&[&row], false, usize::MAX);
        let mut back = Vec::new();
        h.newest().unwrap().decode(6, &mut back);
        assert_eq!(back[0][..], row[..]);
    }
}
