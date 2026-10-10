//! Turns composed frames into the escape sequences sent to a client's terminal.
//!
//! The server renders each client view into a [`Buffer`]; [`Encoder`] diffs it against the last
//! frame sent to that client and emits only the changed cells, wrapped in synchronized-output
//! markers so the outer terminal never shows a half-drawn frame.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use ratatui::buffer::{Buffer, Cell};
use ratatui::style::{Color, Modifier};

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

#[derive(Default)]
pub struct Encoder {
    prev: Option<Buffer>,
    prev_links: Links,
    last_cursor_style: Option<(CursorShape, bool)>,
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

    /// Append the bytes that turn the previous frame into `next` to `out`.
    pub fn encode(&mut self, next: &Buffer, cursor: Option<Cursor>, links: &Links, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        let full = self.prev.as_ref().is_none_or(|p| p.area != next.area);
        let blank;
        let prev = if full {
            out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
            blank = Buffer::empty(next.area);
            &blank
        } else {
            self.prev.as_ref().unwrap()
        };

        let mut updates: Vec<(u16, u16, &Cell)> = prev.diff(next);
        // Cells whose link changed but whose text did not must be drawn again too.
        if !full && self.prev_links != *links {
            let have: std::collections::HashSet<(u16, u16)> = updates.iter().map(|(x, y, _)| (*x, *y)).collect();
            let mut extra: Vec<(u16, u16)> = links
                .iter()
                .filter(|(pos, uri)| self.prev_links.get(pos) != Some(uri))
                .map(|(pos, _)| *pos)
                .chain(self.prev_links.keys().filter(|pos| !links.contains_key(pos)).copied())
                .filter(|pos| !have.contains(pos) && next.area.contains((pos.0, pos.1).into()))
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
        self.prev_links = links.clone();

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
        self.prev = Some(next.clone());
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
    let mut params: Vec<String> = Vec::new();
    if cell.modifier != pen.modifier {
        let removed = pen.modifier - cell.modifier;
        if !removed.is_empty() {
            params.push("0".into());
            *pen = Pen::RESET;
        }
        let added = cell.modifier - pen.modifier;
        for (m, code) in [
            (Modifier::BOLD, "1"),
            (Modifier::DIM, "2"),
            (Modifier::ITALIC, "3"),
            (Modifier::UNDERLINED, "4"),
            (Modifier::SLOW_BLINK, "5"),
            (Modifier::RAPID_BLINK, "6"),
            (Modifier::REVERSED, "7"),
            (Modifier::HIDDEN, "8"),
            (Modifier::CROSSED_OUT, "9"),
        ] {
            if added.contains(m) {
                params.push(code.into());
            }
        }
        pen.modifier = cell.modifier;
    }
    if cell.fg != pen.fg {
        params.push(color_param(cell.fg, Layer::Fg));
        pen.fg = cell.fg;
    }
    if cell.bg != pen.bg {
        params.push(color_param(cell.bg, Layer::Bg));
        pen.bg = cell.bg;
    }
    if cell.underline_color != pen.underline {
        params.push(color_param(cell.underline_color, Layer::Underline));
        pen.underline = cell.underline_color;
    }
    if !params.is_empty() {
        let _ = write!(out, "\x1b[{}m", params.join(";"));
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

fn color_param(c: Color, layer: Layer) -> String {
    match (layer, c) {
        (Layer::Fg, Color::Reset) => "39".into(),
        (Layer::Bg, Color::Reset) => "49".into(),
        (Layer::Underline, Color::Reset) => "59".into(),
        (Layer::Fg, Color::Indexed(i)) => format!("38;5;{i}"),
        (Layer::Bg, Color::Indexed(i)) => format!("48;5;{i}"),
        (Layer::Underline, Color::Indexed(i)) => format!("58;5;{i}"),
        (Layer::Fg, Color::Rgb(r, g, b)) => format!("38;2;{r};{g};{b}"),
        (Layer::Bg, Color::Rgb(r, g, b)) => format!("48;2;{r};{g};{b}"),
        (Layer::Underline, Color::Rgb(r, g, b)) => format!("58;2;{r};{g};{b}"),
        (layer, named) => {
            let i = named_index(named).unwrap_or(7);
            let (base, bright) = match layer {
                Layer::Fg => (30, 90),
                Layer::Bg => (40, 100),
                Layer::Underline => return format!("58;5;{i}"),
            };
            if i < 8 { (base + i).to_string() } else { (bright + i - 8).to_string() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

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
        enc.encode(&buf, None, &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("\x1b[2J"));
        assert!(s.contains("\x1b[31mhello"));

        buf.set_string(1, 0, "a", Style::default().fg(Color::Red));
        let mut out = Vec::new();
        enc.encode(&buf, None, &Links::new(), &mut out);
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
        Encoder::new().encode(&buf, None, &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("\x1b[1ma\x1b[0mb"), "{s:?}");
    }

    #[test]
    fn wide_chars_advance_two_columns() {
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "日x", Style::default());
        let mut out = Vec::new();
        Encoder::new().encode(&buf, None, &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("日x"), "{s:?}");
    }

    #[test]
    fn cursor_is_placed_and_shaped_once() {
        let buf = Buffer::empty(Rect::new(0, 0, 4, 4));
        let mut enc = Encoder::new();
        let cur = Cursor { x: 2, y: 3, shape: CursorShape::Bar, blinking: false };
        let mut out = Vec::new();
        enc.encode(&buf, Some(cur), &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("\x1b[6 q\x1b[4;3H\x1b[?25h"));
        let mut out = Vec::new();
        enc.encode(&buf, Some(cur), &Links::new(), &mut out);
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
        enc.encode(&buf, None, &links, &mut out);
        let s = text(&out);
        assert!(s.contains("\x1b]8;;https://example.org\x1b\\\x1b[1;4Hhere\x1b]8;;\x1b\\"), "{s:?}");
        assert!(s.find("go").unwrap() < s.find("]8;;https").unwrap(), "text before the link is not linked");
        // Same text, link removed: the cells are drawn again without it.
        let mut out = Vec::new();
        enc.encode(&buf, None, &Links::new(), &mut out);
        let s = text(&out);
        assert!(s.contains("here") && !s.contains("example.org"), "{s:?}");
    }

    #[test]
    fn colors_map_to_sgr() {
        assert_eq!(color_param(Color::LightBlue, Layer::Fg), "94");
        assert_eq!(color_param(Color::Green, Layer::Bg), "42");
        assert_eq!(color_param(Color::Indexed(200), Layer::Fg), "38;5;200");
        assert_eq!(color_param(Color::Rgb(1, 2, 3), Layer::Bg), "48;2;1;2;3");
    }
}
