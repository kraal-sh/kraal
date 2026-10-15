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

/// Leave one column for the cursor while showing the preceding characters that fit.
pub fn input_window(chars: &[char], cursor: usize, room: usize) -> (String, usize) {
    let (mut start, mut before) = (cursor.min(chars.len()), 0);
    while start > 0 {
        let width = chars[start - 1].width().unwrap_or(0);
        if before + width >= room {
            break;
        }
        before += width;
        start -= 1;
    }
    (chars[start..].iter().collect(), before)
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

/// The first line of `text`, cut to `width` columns, then ` …` when more lines follow.
pub fn first_line(text: &str, width: usize) -> String {
    let first = truncate(text.lines().next().unwrap_or("").trim(), width);
    if text.trim().lines().count() > 1 { format!("{first} …") } else { first }
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

/// Entry `number` of a list of commands shown to be approved: every line of `code` as it is
/// written, the first after the number and the others under it.
pub fn numbered(number: usize, code: &str) -> Vec<String> {
    let label = format!("  {number}. ");
    let under = " ".repeat(label.len());
    code.lines().enumerate().map(|(i, line)| format!("{}{line}", if i == 0 { &label } else { &under })).collect()
}

/// Cuts `line` into rows of `width` columns as it is written: unlike [`wrap`], every space is
/// kept and a row ends where the room does. The rows after the first start under the line's
/// indentation (at most half the width), so that they read as the rest of it.
pub fn fold(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let indent = line.chars().take_while(|c| *c == ' ').count().min(width / 2);
    let mut rows = vec![String::new()];
    let (mut used, mut start) = (0, 0);
    for c in line.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && used > start {
            rows.push(" ".repeat(indent));
            (used, start) = (indent, indent);
        }
        if let Some(row) = rows.last_mut() {
            row.push(c);
        }
        used += w;
    }
    rows
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
    #[allow(
        deprecated,
        reason = "on musl, the libc crate warns that time_t will become 64-bit: it has been since musl 1.2"
    )]
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
    fn input_windows_preserve_wide_and_combining_characters_and_reserve_the_cursor() {
        for (value, cursor, room, shown, column) in [
            ("abcdef", 4, 4, "bcdef", 3),
            ("abc日e\u{301}Z", 6, 4, "日e\u{301}Z", 3),
            ("abc日e\u{301}Z", 6, 3, "e\u{301}Z", 1),
            ("abc日e\u{301}Z", 4, 3, "日e\u{301}Z", 2),
            ("abc", 0, 2, "abc", 0),
            ("abc", 99, 2, "c", 1),
            ("abc", 2, 0, "c", 0),
            ("日", 1, 1, "", 0),
            ("", 99, 0, "", 0),
        ] {
            assert_eq!(input_window(&value.chars().collect::<Vec<_>>(), cursor, room), (shown.into(), column));
        }
    }

    #[test]
    fn a_wall_clock_time_read_as_local_shows_as_itself() {
        // 2024-02-29 00:59:59 in UTC, then the instant that reads so in local time.
        let wall = 1_709_168_399;
        let offset = local_tm(wall).map_or(0, |tm| tm.tm_gmtoff);
        assert_eq!(local_minutes(wall - offset), "2024-02-29 00:59");
    }

    #[test]
    fn a_first_line_is_cut_to_columns() {
        assert_eq!(first_line("  make  ", 80), "make");
        assert_eq!(first_line("make\nmake install\n", 80), "make …");
        let wide = "漢字".repeat(10);
        assert_eq!(first_line(&wide, 7), "漢字漢…");
        assert_eq!(first_line(&format!("{wide}\nmore"), 7), "漢字漢… …");
    }

    #[test]
    fn a_numbered_command_keeps_its_lines_under_its_number() {
        assert_eq!(numbered(3, "make"), ["  3. make"]);
        assert_eq!(numbered(10, "if x; then\n  y\n\nfi"), ["  10. if x; then", "        y", "      ", "      fi"]);
    }

    #[test]
    fn a_folded_line_keeps_every_character_and_its_indentation() {
        assert_eq!(fold("  if  x;  then", 20), ["  if  x;  then"]);
        assert_eq!(fold("    echo one  two", 10), ["    echo o", "    ne  tw", "    o"]);
        assert_eq!(fold("  ", 10), ["  "], "a blank line of code is a row");
        // Wide characters are not split, and a deep indentation leaves room for the text.
        assert_eq!(fold(" 漢字漢字", 5), [" 漢字", " 漢字"]);
        assert_eq!(fold("      ab", 4), ["    ", "    ", "  ab"]);
        assert_eq!(fold("abc", 0), ["a", "b", "c"]);
    }
}
