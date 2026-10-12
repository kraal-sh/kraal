//! Sixel images (DEC's format, as img2sixel, chafa or matplotlib backends send them): measured
//! as their data comes, so that their cells can be taken at once, then painted into RGBA pixels
//! off the server's loop; and encoded for terminals that draw sixel but not kitty's placeholders.
//! Pixels no sixel sets stay transparent, so the terminal's background shows.

use std::io::Write;

use super::quantize::{CLEAR, Indexed};
use super::{MAX_SIDE, within_limits};

/// Colour registers a picture may define.
const REGISTERS: usize = 1024;

/// The VT340's default palette, for pictures that use colours without defining them.
const VT340: [[u8; 3]; 16] = [
    [0, 0, 0],
    [51, 51, 204],
    [204, 33, 33],
    [51, 204, 51],
    [204, 51, 204],
    [51, 204, 204],
    [204, 204, 51],
    [120, 120, 120],
    [69, 69, 69],
    [87, 87, 153],
    [153, 69, 69],
    [87, 153, 87],
    [153, 87, 153],
    [87, 153, 153],
    [153, 153, 87],
    [204, 204, 204],
];

/// The bytes that are sixels: each sets pixels of a column six high, as its bits (less `?`) say,
/// from the top one.
const SIXELS: std::ops::RangeInclusive<u8> = b'?'..=b'~';

/// What sixel data does, in order.
enum Op<'a> {
    /// Announce the picture's size.
    Raster { width: u32, height: u32 },
    /// Draw with a colour register, after giving it a colour when `rgb` says one.
    Colour { register: usize, rgb: Option<[u8; 3]> },
    /// Set the columns of a band from column `x`, a sixel each.
    Sixels { x: u32, band: u32, sixels: &'a [u8] },
    /// Set `count` columns of a band from column `x` with one sixel.
    Repeat { x: u32, band: u32, count: u32, sixel: u8 },
}

/// Reads the data of a sixel image (what follows `q`) a piece at a time: a piece may end in the
/// middle of an introducer's parameters.
#[derive(Default)]
struct Reader {
    /// The introducer (`"`, `#` or `!`) whose parameters are being read.
    introducer: Option<u8>,
    /// Its parameters so far: `"` and `#` take up to five, `!` one.
    params: [u32; 5],
    len: usize,
    x: u32,
    band: u32,
}

impl Reader {
    fn read<'a>(&mut self, data: &'a [u8], each: &mut impl FnMut(Op<'a>)) {
        let mut i = 0;
        while i < data.len() {
            if self.introducer.is_some() && self.param(data[i], each) {
                i += 1;
                continue;
            }
            let run = data[i..].iter().take_while(|b| SIXELS.contains(*b)).count();
            if run > 0 {
                each(Op::Sixels { x: self.x, band: self.band, sixels: &data[i..i + run] });
                self.x = self.x.saturating_add(run as u32);
                i += run;
                continue;
            }
            match data[i] {
                introducer @ (b'"' | b'#' | b'!') => {
                    self.introducer = Some(introducer);
                    self.params[0] = 0;
                    self.len = 1;
                }
                b'$' => self.x = 0,
                b'-' => {
                    self.x = 0;
                    self.band = self.band.saturating_add(1);
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// A byte after an introducer: false when it ends the parameters, and is read as data.
    fn param<'a>(&mut self, b: u8, each: &mut impl FnMut(Op<'a>)) -> bool {
        match b {
            b'0'..=b'9' => {
                let n = &mut self.params[self.len - 1];
                *n = n.saturating_mul(10).saturating_add(u32::from(b - b'0'));
            }
            b';' if self.introducer != Some(b'!') && self.len < self.params.len() => {
                self.params[self.len] = 0;
                self.len += 1;
            }
            sixel if self.introducer == Some(b'!') && SIXELS.contains(&sixel) => {
                self.introducer = None;
                let count = self.params[0].max(1);
                each(Op::Repeat { x: self.x, band: self.band, count, sixel });
                self.x = self.x.saturating_add(count);
            }
            _ => {
                self.end_params(each);
                return false;
            }
        }
        true
    }

    /// The parameters of the introducer being read are all there: a repeat count that no sixel
    /// follows is dropped.
    fn end_params<'a>(&mut self, each: &mut impl FnMut(Op<'a>)) {
        match (self.introducer.take(), &self.params[..self.len]) {
            (Some(b'"'), &[_, _, width, height, ..]) => each(Op::Raster { width, height }),
            (Some(b'#'), &[register, ref rgb @ ..]) => {
                let rgb = match *rgb {
                    [1, h, l, s] => Some(hls(h, l, s)),
                    [_, r, g, b] => Some([percent(r), percent(g), percent(b)]),
                    _ => None,
                };
                each(Op::Colour { register: register as usize % REGISTERS, rgb });
            }
            _ => {}
        }
    }
}

/// The size of a sixel image, measured as its data comes in pieces.
#[derive(Default)]
pub struct Measure {
    reader: Reader,
    extent: Extent,
}

impl Measure {
    pub fn read(&mut self, data: &[u8]) {
        let extent = &mut self.extent;
        self.reader.read(data, &mut |op| extent.take(op));
    }

    /// The picture's size in pixels, once all its data was read: `None` when it draws nothing
    /// or is larger than the limits.
    pub fn size(mut self) -> Option<(u32, u32)> {
        // Data that ends with raster attributes still announces a size.
        let extent = &mut self.extent;
        self.reader.end_params(&mut |op| extent.take(op));
        self.extent.size()
    }
}

/// The size a picture announced, and how far its sixels reach.
#[derive(Default)]
struct Extent {
    announced: (u32, u32),
    width: u32,
    height: u32,
    /// A sixel sets pixels beyond the limits.
    over: bool,
}

impl Extent {
    fn take(&mut self, op: Op<'_>) {
        match op {
            Op::Raster { width, height } => self.announced = (width.min(MAX_SIDE), height.min(MAX_SIDE)),
            Op::Colour { .. } => {}
            Op::Sixels { x, band, sixels } => {
                let set = sixels.iter().rposition(|s| *s != b'?').map(|last| {
                    let bits = sixels[..=last].iter().fold(0, |bits, s| bits | (s - b'?'));
                    (last as u64 + 1, bits)
                });
                self.reach(x, band, sixels.len() as u64, set);
            }
            Op::Repeat { x, band, count, sixel } => {
                let set = (sixel != b'?').then_some((u64::from(count), sixel - b'?'));
                self.reach(x, band, u64::from(count), set);
            }
        }
    }

    /// Sixels over `columns` columns of a band from `x`: blank ones widen the picture as far as
    /// it may go; those that set pixels, over the first `set.0` columns, its bits `set.1` from
    /// the top, must fit in it.
    fn reach(&mut self, x: u32, band: u32, columns: u64, set: Option<(u64, u8)>) {
        self.width = self.width.max((u64::from(x) + columns).min(u64::from(MAX_SIDE)) as u32);
        let Some((columns, bits)) = set else { return };
        let right = u64::from(x) + columns;
        let bottom = u64::from(band) * 6 + u64::from(u8::BITS - bits.leading_zeros());
        if right > u64::from(MAX_SIDE) || bottom > u64::from(MAX_SIDE) {
            self.over = true;
            return;
        }
        self.width = self.width.max(right as u32);
        self.height = self.height.max(bottom as u32);
    }

    fn size(&self) -> Option<(u32, u32)> {
        let (width, height) = (self.width.max(self.announced.0), self.height.max(self.announced.1));
        (!self.over && within_limits(width, height)).then_some((width, height))
    }
}

/// Paint the data of a sixel image into the `width × height` RGBA pixels [`Measure`] found.
pub fn paint(data: &[u8], (width, height): (u32, u32)) -> Vec<u8> {
    let mut canvas = Canvas::new(width as usize, height as usize);
    Reader::default().read(data, &mut |op| canvas.draw(op));
    canvas.pixels
}

/// The data of a sixel image measured, then painted: its size and its RGBA pixels.
#[cfg(test)]
pub fn decode(data: &[u8]) -> Option<((u32, u32), Vec<u8>)> {
    let mut measure = Measure::default();
    measure.read(data);
    let size = measure.size()?;
    Some((size, paint(data, size)))
}

/// RGBA pixels being painted, and the colours they are painted with.
struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    palette: Vec<[u8; 3]>,
    colour: [u8; 4],
}

impl Canvas {
    fn new(width: usize, height: usize) -> Canvas {
        let mut palette = vec![[0u8; 3]; REGISTERS];
        palette[..16].copy_from_slice(&VT340);
        let [r, g, b] = palette[0];
        Canvas { width, height, pixels: vec![0; width * height * 4], palette, colour: [r, g, b, 255] }
    }

    fn draw(&mut self, op: Op<'_>) {
        match op {
            Op::Raster { .. } => {}
            Op::Colour { register, rgb } => {
                if let Some(rgb) = rgb {
                    self.palette[register] = rgb;
                }
                let [r, g, b] = self.palette[register];
                self.colour = [r, g, b, 255];
            }
            Op::Sixels { x, band, sixels } => {
                for (x, sixel) in (x as usize..).zip(sixels) {
                    self.set(x, band, 1, *sixel);
                }
            }
            Op::Repeat { x, band, count, sixel } => self.set(x as usize, band, count as usize, sixel),
        }
    }

    /// Set `count` columns of `band` from column `x` as `sixel` says.
    fn set(&mut self, x: usize, band: u32, count: usize, sixel: u8) {
        let right = x.saturating_add(count).min(self.width);
        let x = x.min(right);
        let bits = sixel - b'?';
        let rows = (0..6).filter(|bit| bits & (1 << bit) != 0).map(|bit| band as usize * 6 + bit);
        for y in rows.take_while(|y| *y < self.height) {
            let row = y * self.width;
            for pixel in self.pixels[(row + x) * 4..(row + right) * 4].as_chunks_mut::<4>().0 {
                *pixel = self.colour;
            }
        }
    }
}

/// `n` percent of 255.
fn percent(n: u32) -> u8 {
    (n.min(100) * 255 / 100) as u8
}

/// DEC's HLS: hue 0 is blue and 120 red (standard HLS has red at 0), lightness and saturation
/// in percent.
fn hls(h: u32, l: u32, s: u32) -> [u8; 3] {
    let h = ((h % 360) + 240) % 360;
    let (l, s) = (f64::from(l.min(100)) / 100.0, f64::from(s.min(100)) / 100.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = f64::from(h) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match h / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [to(r), to(g), to(b)]
}

/// Encode the `width × height` pixels of `image` from `x, y` as a sixel image (DCS … ST), drawn
/// at the cursor. Unset ([`CLEAR`]) pixels stay transparent; colours are defined as the part
/// uses them, in the image's palette numbering, so that every part of an image matches.
pub fn encode(image: &Indexed, x: u32, y: u32, width: u32, height: u32, out: &mut Vec<u8>) {
    let w = width.min(image.width.saturating_sub(x)) as usize;
    let h = height.min(image.height.saturating_sub(y)) as usize;
    if w == 0 || h == 0 {
        return;
    }
    let stride = image.width as usize;
    let row = |r: usize| {
        let at = (y as usize + r) * stride + x as usize;
        &image.pixels[at..at + w]
    };
    // P2 = 1: what no sixel sets keeps what is under it. Raster attributes: square pixels and
    // the size, so that the terminal knows it before the data.
    let _ = write!(out, "\x1bP0;1;0q\"1;1;{w};{h}");
    let mut used = [false; 256];
    for r in 0..h {
        for &p in row(r) {
            used[usize::from(p)] = true;
        }
    }
    for (i, [r, g, b]) in image.palette.iter().enumerate().take(usize::from(CLEAR)) {
        if used[i] {
            let pct = |v: u8| (u32::from(v) * 100 + 127) / 255;
            let _ = write!(out, "#{i};2;{};{};{}", pct(*r), pct(*g), pct(*b));
        }
    }
    // For each colour of a band, its six-pixel columns and the span they cover.
    let mut masks = vec![0u8; 256 * w];
    let mut span = [(usize::MAX, 0usize); 256];
    let mut present: Vec<u8> = Vec::with_capacity(256);
    for band in 0..h.div_ceil(6) {
        if band > 0 {
            out.push(b'-');
        }
        present.clear();
        for bit in 0..6 {
            let r = band * 6 + bit;
            if r >= h {
                break;
            }
            for (px, &p) in row(r).iter().enumerate() {
                if p == CLEAR {
                    continue;
                }
                let c = usize::from(p);
                if span[c].0 == usize::MAX {
                    present.push(p);
                    span[c] = (px, px);
                } else {
                    span[c].0 = span[c].0.min(px);
                    span[c].1 = span[c].1.max(px);
                }
                masks[c * w + px] |= 1 << bit;
            }
        }
        for (k, &p) in present.iter().enumerate() {
            let c = usize::from(p);
            if k > 0 {
                out.push(b'$');
            }
            let _ = write!(out, "#{c}");
            let (from, to) = span[c];
            if from > 0 {
                run(out, from, 0);
            }
            let bits = &mut masks[c * w + from..=c * w + to];
            let mut i = 0;
            while i < bits.len() {
                let v = bits[i];
                let n = bits[i..].iter().take_while(|b| **b == v).count();
                run(out, n, v);
                i += n;
            }
            bits.fill(0);
            span[c] = (usize::MAX, 0);
        }
    }
    out.extend_from_slice(b"\x1b\\");
}

/// `n` sixels of `bits`, repeated with `!` when that is shorter.
fn run(out: &mut Vec<u8>, n: usize, bits: u8) {
    let c = 0x3f + bits;
    if n > 3 {
        let _ = write!(out, "!{n}");
        out.push(c);
    } else {
        out.extend(std::iter::repeat_n(c, n));
    }
}
