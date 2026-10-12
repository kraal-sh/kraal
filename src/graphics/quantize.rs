//! Images fitted to the cells they fill and reduced to a palette: what sixel draws.

use std::collections::HashMap;

use image::RgbaImage;
use image::imageops::{self, FilterType};

/// The palette index of a pixel left transparent.
pub const CLEAR: u8 = 255;
/// Colours in a palette: every index but [`CLEAR`]. Terminals that draw sixel have at least 256
/// colour registers.
const COLORS: usize = 255;

/// `width * height` palette indices.
pub struct Indexed {
    pub width: u32,
    pub height: u32,
    pub palette: Vec<[u8; 3]>,
    pub pixels: Vec<u8>,
}

impl Indexed {
    /// Memory it takes.
    pub fn bytes(&self) -> usize {
        self.pixels.len() + self.palette.len() * 3
    }
}

/// `rgba` in a box of `width × height` pixels as terminals place an image in its cells: scaled
/// to fit, its proportions kept, centred, the rest of the box transparent.
pub fn fit(rgba: &RgbaImage, width: u32, height: u32) -> Indexed {
    let mut out = Indexed { width, height, palette: Vec::new(), pixels: vec![CLEAR; width as usize * height as usize] };
    let (iw, ih) = rgba.dimensions();
    if iw == 0 || ih == 0 || width == 0 || height == 0 {
        return out;
    }
    let scale = (f64::from(width) / f64::from(iw)).min(f64::from(height) / f64::from(ih));
    let sw = ((f64::from(iw) * scale).round() as u32).clamp(1, width);
    let sh = ((f64::from(ih) * scale).round() as u32).clamp(1, height);
    let resized;
    let scaled = if (sw, sh) == (iw, ih) {
        rgba
    } else {
        resized = if sw < iw && sh < ih {
            imageops::thumbnail(rgba, sw, sh)
        } else {
            imageops::resize(rgba, sw, sh, FilterType::Triangle)
        };
        &resized
    };
    let (palette, indices) = quantize(scaled.as_raw());
    let (ox, oy) = (((width - sw) / 2) as usize, ((height - sh) / 2) as usize);
    let (sw, width) = (sw as usize, width as usize);
    for (row, line) in indices.chunks_exact(sw).enumerate() {
        let at = (oy + row) * width + ox;
        out.pixels[at..at + sw].copy_from_slice(line);
    }
    out.palette = palette;
    out
}

fn opaque(p: &[u8]) -> bool {
    p[3] >= 128
}

/// A palette for RGBA pixels, and each pixel's index in it ([`CLEAR`] for transparent ones).
/// Pictures with few colours (plots, diagrams, screenshots) keep their own; others get those
/// NeuQuant learns from them.
pub fn quantize(rgba: &[u8]) -> (Vec<[u8; 3]>, Vec<u8>) {
    if let Some(found) = exact(rgba) {
        return found;
    }
    let all_opaque = rgba.chunks_exact(4).all(opaque);
    let sample: Vec<u8>;
    let learn = if all_opaque {
        rgba
    } else {
        sample = rgba.chunks_exact(4).filter(|p| opaque(p)).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
        &sample
    };
    // NeuQuant learns from one pixel in `samplefac`: some fifty thousand at most, for speed.
    let samplefac = (learn.len() / 4 / 50_000).clamp(10, 30) as i32;
    let nq = color_quant::NeuQuant::new(samplefac, COLORS, learn);
    let palette = (0..COLORS).map(|i| nq.lookup(i).map_or([0; 3], |[r, g, b, _]| [r, g, b])).collect();
    // Colours looked up once, by their six high bits per channel: far finer than the palette.
    let mut table = vec![CLEAR; 1 << 18];
    let indices = rgba
        .chunks_exact(4)
        .map(|p| {
            if !opaque(p) {
                return CLEAR;
            }
            let key = usize::from(p[0] >> 2) << 12 | usize::from(p[1] >> 2) << 6 | usize::from(p[2] >> 2);
            if table[key] == CLEAR {
                let mid = |v: u8| (v & !3) | 2;
                table[key] = nq.index_of(&[mid(p[0]), mid(p[1]), mid(p[2]), 255]) as u8;
            }
            table[key]
        })
        .collect();
    (palette, indices)
}

/// The palette of a picture with no more than [`COLORS`] colours.
fn exact(rgba: &[u8]) -> Option<(Vec<[u8; 3]>, Vec<u8>)> {
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut seen: HashMap<[u8; 3], u8> = HashMap::new();
    let mut last: Option<([u8; 3], u8)> = None;
    let mut indices = Vec::with_capacity(rgba.len() / 4);
    for p in rgba.chunks_exact(4) {
        if !opaque(p) {
            indices.push(CLEAR);
            continue;
        }
        let c = [p[0], p[1], p[2]];
        let i = match last {
            Some((l, i)) if l == c => i,
            _ => match seen.get(&c) {
                Some(i) => *i,
                None if palette.len() == COLORS => return None,
                None => {
                    let i = palette.len() as u8;
                    palette.push(c);
                    seen.insert(c, i);
                    i
                }
            },
        };
        last = Some((c, i));
        indices.push(i);
    }
    Some((palette, indices))
}
