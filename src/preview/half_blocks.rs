//! Images drawn with Unicode half blocks: two pixels per cell.

use image::imageops::FilterType;
use image::{Rgba, RgbaImage, imageops};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

/// Scales `image` to fit `max_width`×`max_height` pixels, keeping its aspect ratio.
pub(super) fn fit_image(image: &RgbaImage, max_width: u32, max_height: u32) -> RgbaImage {
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
pub(super) fn draw_half_blocks(pixels: &RgbaImage, area: Rect, buf: &mut Buffer) {
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

pub(super) fn opaque(pixel: &Rgba<u8>) -> Option<Color> {
    let [r, g, b, a] = pixel.0;
    (a >= 128).then_some(Color::Rgb(r, g, b))
}
