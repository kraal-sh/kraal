//! Sixel images (DEC's format, as img2sixel, chafa or matplotlib backends send them): decoded
//! into RGBA pixels, and encoded for terminals that draw sixel but not kitty's placeholders.
//! Pixels no sixel sets stay transparent, so the terminal's background shows.

use std::io::Write;

use super::quantize::{CLEAR, Indexed};

/// Largest image decoded, per side and in pixels.
pub const MAX_SIDE: u32 = 10_000;
pub const MAX_PIXELS: u64 = 40_000_000;
/// Colour registers a picture may define.
const REGISTERS: usize = 1024;

/// A decoded picture: `width * height` RGBA pixels.
#[derive(Debug, PartialEq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

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

/// Decode the data of a sixel DCS (what follows `q`). `None` when it draws nothing or is
/// larger than the limits.
pub fn decode(data: &[u8]) -> Option<Rgba> {
    let mut palette = vec![[0u8, 0, 0]; REGISTERS];
    palette[..16].copy_from_slice(&VT340);
    let mut color = palette[0];
    let mut canvas = Canvas::default();
    let (mut x, mut band) = (0u32, 0u32);
    let mut i = 0;
    let number = |i: &mut usize| -> u32 {
        let mut n: u32 = 0;
        while *i < data.len() && data[*i].is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add(u32::from(data[*i] - b'0'));
            *i += 1;
        }
        n
    };
    // Up to five `;`-separated numbers after an introducer.
    let numbers = |i: &mut usize| -> Vec<u32> {
        let mut out = vec![number(i)];
        while *i < data.len() && data[*i] == b';' && out.len() < 5 {
            *i += 1;
            out.push(number(i));
        }
        out
    };
    while i < data.len() {
        let b = data[i];
        i += 1;
        match b {
            // Raster attributes: aspect ratio, then the size the picture announces.
            b'"' => {
                let n = numbers(&mut i);
                if let (Some(&w), Some(&h)) = (n.get(2), n.get(3)) {
                    canvas.reserve(w.min(MAX_SIDE), h.min(MAX_SIDE))?;
                    canvas.announced = (w.min(MAX_SIDE), h.min(MAX_SIDE));
                }
            }
            b'#' => {
                let n = numbers(&mut i);
                let register = n[0] as usize % REGISTERS;
                if n.len() >= 5 {
                    palette[register] = match n[1] {
                        1 => hls(n[2], n[3], n[4]),
                        _ => [percent(n[2]), percent(n[3]), percent(n[4])],
                    };
                }
                color = palette[register];
            }
            b'!' => {
                let count = number(&mut i);
                if i < data.len() && (0x3f..=0x7e).contains(&data[i]) {
                    let bits = data[i] - 0x3f;
                    i += 1;
                    canvas.paint(x, band, count.max(1), bits, color)?;
                    x = x.saturating_add(count.max(1));
                }
            }
            b'$' => x = 0,
            b'-' => {
                x = 0;
                band += 1;
            }
            0x3f..=0x7e => {
                canvas.paint(x, band, 1, b - 0x3f, color)?;
                x += 1;
            }
            _ => {}
        }
    }
    canvas.finish()
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

/// The pixels painted so far; grows as the picture goes right and down.
#[derive(Default)]
struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    /// The rightmost and lowest pixels painted, and the size the picture announced.
    used_w: u32,
    used_h: u32,
    announced: (u32, u32),
}

impl Canvas {
    /// Make room for `w × h` pixels; `None` past the limits.
    fn reserve(&mut self, w: u32, h: u32) -> Option<()> {
        if w <= self.width && h <= self.height {
            return Some(());
        }
        let (nw, nh) = (w.max(self.width), h.max(self.height));
        if nw > MAX_SIDE || nh > MAX_SIDE || u64::from(nw) * u64::from(nh) > MAX_PIXELS {
            return None;
        }
        let mut pixels = vec![0u8; nw as usize * nh as usize * 4];
        for row in 0..self.height as usize {
            let from = row * self.width as usize * 4;
            let to = row * nw as usize * 4;
            pixels[to..to + self.width as usize * 4]
                .copy_from_slice(&self.pixels[from..from + self.width as usize * 4]);
        }
        self.pixels = pixels;
        self.width = nw;
        self.height = nh;
        Some(())
    }

    /// Paint the six pixels of `bits` in band `band`, `count` times from column `x`.
    fn paint(&mut self, x: u32, band: u32, count: u32, bits: u8, color: [u8; 3]) -> Option<()> {
        let top = band.checked_mul(6)?;
        let right = x.checked_add(count)?;
        if bits == 0 {
            // Nothing painted, but the picture is that wide.
            self.used_w = self.used_w.max(right.min(MAX_SIDE));
            return Some(());
        }
        // Grow in steps, not one column at a time.
        let grow = |have: u32, need: u32| {
            if need > have { need.max(have.saturating_mul(2)).min(MAX_SIDE.max(need)) } else { have }
        };
        self.reserve(grow(self.width, right), grow(self.height, top + 6))?;
        for bit in 0..6 {
            if bits & (1 << bit) == 0 {
                continue;
            }
            let y = top + bit;
            let row = y as usize * self.width as usize * 4;
            for px in x..right {
                let at = row + px as usize * 4;
                self.pixels[at..at + 4].copy_from_slice(&[color[0], color[1], color[2], 255]);
            }
            self.used_h = self.used_h.max(y + 1);
        }
        self.used_w = self.used_w.max(right);
        Some(())
    }

    /// The painted part of the canvas, or the announced size if larger.
    fn finish(self) -> Option<Rgba> {
        let w = self.used_w.max(self.announced.0).min(self.width);
        let h = self.used_h.max(self.announced.1).min(self.height);
        if w == 0 || h == 0 {
            return None;
        }
        let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
        for row in 0..h as usize {
            let from = row * self.width as usize * 4;
            pixels.extend_from_slice(&self.pixels[from..from + w as usize * 4]);
        }
        Some(Rgba { width: w, height: h, pixels })
    }
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
