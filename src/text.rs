//! Text as kraal shows it: control characters made visible, widths in terminal columns,
//! truncation and wrapping, sizes and times.

use std::iter;
use std::mem;
use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::UnicodeWidthChar;

/// Formats a byte count with binary units, e.g. `1.5 KiB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
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

/// Replaces control characters with visible placeholders.
pub fn visible_char(c: char) -> char {
    match c {
        '\0'..='\x1f' => char::from_u32(0x2400 + u32::from(c)).unwrap_or('\u{fffd}'),
        '\x7f' => '\u{2421}',
        '\u{80}'..='\u{9f}' => '\u{fffd}',
        c => c,
    }
}

pub fn sanitize(text: &str) -> String {
    text.chars().map(visible_char).collect()
}

/// Like [`sanitize`], keeping line breaks.
pub fn sanitize_lines(text: &str) -> String {
    text.chars().map(|c| if c == '\n' { c } else { visible_char(c) }).collect()
}

pub fn columns(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Cuts `text` to `width` columns, ending with `…` when shortened.
pub fn truncate(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let kept = text.chars().take_while(|c| {
        used += c.width().unwrap_or(0);
        used < width
    });
    kept.chain(iter::once('…')).collect()
}

/// Cuts the start of `text` to `width` columns, starting with `…` when shortened.
pub fn truncate_start(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let mut kept: Vec<char> = text
        .chars()
        .rev()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used < width
        })
        .collect();
    kept.push('…');
    kept.into_iter().rev().collect()
}

/// Right-aligns `text` in `width` columns.
pub fn pad_start(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    format!("{}{text}", " ".repeat(width.saturating_sub(columns(&text))))
}

/// Word-wraps `text` to `width` columns; words longer than a line are split.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for source in text.lines() {
        let mut line = String::new();
        let mut used = 0;
        for word in source.split(' ').filter(|word| !word.is_empty()) {
            let word_width = columns(word);
            if used > 0 && used + 1 + word_width <= width {
                line.push(' ');
                used += 1;
            } else if used > 0 {
                lines.push(mem::take(&mut line));
                used = 0;
            }
            for c in word.chars() {
                let w = c.width().unwrap_or(0);
                if used + w > width && used > 0 {
                    lines.push(mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += w;
            }
        }
        lines.push(line);
    }
    lines
}

/// 64-bit FNV-1a of `text`: stable across runs and versions (unlike std's hasher), to tell
/// names apart in file names. Not a defence against someone crafting collisions.
pub fn fnv64(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h: u64, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

// ---- time ---------------------------------------------------------------------------------

/// Seconds since the epoch (negative before it).
pub fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |secs| -secs),
    }
}

/// The local time of `secs` since the epoch, as `localtime_r` gives it.
fn local_tm(secs: i64) -> Option<libc::tm> {
    // The libc crate warns that musl's time_t will become 64-bit: it is already for musl 1.2.
    #[allow(deprecated)]
    let t = libc::time_t::try_from(secs).ok()?;
    // SAFETY: localtime_r only writes to the struct we own.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    ok.then_some(tm)
}

/// `2026-10-11 14:03:22` in local time.
pub fn local_datetime(secs: i64) -> String {
    match local_tm(secs) {
        Some(tm) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        None => secs.to_string(),
    }
}

/// `2026-10-11 14:03` in local time.
pub fn local_minutes(secs: i64) -> String {
    let mut s = local_datetime(secs);
    if s.len() == 19 {
        s.truncate(16);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_clock_time_read_as_local_shows_as_itself() {
        // 2024-02-29 00:59:59 in UTC, then the instant that reads so in local time.
        let wall = 1_709_168_399;
        let offset = local_tm(wall).map_or(0, |tm| tm.tm_gmtoff);
        assert_eq!(local_minutes(wall - offset), "2024-02-29 00:59");
    }
}
