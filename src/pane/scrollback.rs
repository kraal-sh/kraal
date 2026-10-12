//! A pane's text saved with its session, so that the pane replacing it after a restart shows it
//! again: the primary screen and its history, as lines of text with their attributes (SGR).
//!
//! A line the emulator wrapped is saved whole, so that the emulator it goes back to wraps it at
//! its own width. The alternate screen, where full-screen programs draw, is never saved.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use alacritty_terminal::vte::{Params, Parser, Perform};

use super::*;

/// The most a pane's saved text takes: older lines are left out.
pub const MAX_SAVED_BYTES: usize = 4 << 20;
/// Rows of one wrapped line saved together; the rows before are saved as a line of their own.
const MAX_WRAPPED_ROWS: i32 = 1000;
/// Links longer than this are saved as plain text.
const MAX_LINK: usize = 2048;

/// The attributes saved with the text, and their SGR parameters.
const ATTRIBUTES: [(Flags, &str); 11] = [
    (Flags::BOLD, "1"),
    (Flags::DIM, "2"),
    (Flags::ITALIC, "3"),
    (Flags::UNDERLINE, "4"),
    (Flags::DOUBLE_UNDERLINE, "4:2"),
    (Flags::UNDERCURL, "4:3"),
    (Flags::DOTTED_UNDERLINE, "4:4"),
    (Flags::DASHED_UNDERLINE, "4:5"),
    (Flags::INVERSE, "7"),
    (Flags::HIDDEN, "8"),
    (Flags::STRIKEOUT, "9"),
];

/// Those attributes together.
const STYLE: Flags = {
    let mut all = Flags::empty();
    let mut i = 0;
    while i < ATTRIBUTES.len() {
        all = all.union(ATTRIBUTES[i].0);
        i += 1;
    }
    all
};

impl Pane {
    /// The primary screen and its history as text with its attributes, the oldest lines left
    /// out past [`MAX_SAVED_BYTES`]. None while a full-screen program shows the alternate screen
    /// (the primary one is out of reach then), and for panes that are not terminals.
    pub fn scrollback_ansi(&self) -> Option<Vec<u8>> {
        let t = self.term()?;
        if t.term.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        Some(encode(&t.term, MAX_SAVED_BYTES))
    }

    /// Show the last `lines` lines of `saved` (as [`Pane::scrollback_ansi`] wrote it), then a dim
    /// line saying when it was saved (`saved_at`, seconds since the epoch): the program's output
    /// comes below. Whether there was anything to show.
    pub fn restore_scrollback(&mut self, saved: &[u8], saved_at: Option<i64>, lines: usize) -> bool {
        let Content::Term(t) = &mut self.content else { return false };
        let mut bytes = clean(saved, lines);
        if bytes.is_empty() {
            return false;
        }
        let marker = match saved_at {
            Some(at) => format!("kraal: restored from {}", crate::text::local_minutes(at)),
            None => "kraal: restored".to_string(),
        };
        bytes.extend_from_slice(format!("\x1b[0m\r\n\x1b[2m{marker}\x1b[0m\r\n").as_bytes());
        t.advance(&bytes);
        // Nothing in it asks for an answer: whatever the emulator made of it is not the program's.
        t.events.borrow_mut().clear();
        self.restored = Some(marker);
        true
    }

    /// The screen's text below the line that marks restored output, while that line shows: what
    /// runs in the pane wrote it, not a program before the restart. The whole screen otherwise.
    pub fn live_text(&self) -> String {
        if let (Some(marker), Some(t)) = (&self.restored, self.term())
            && let Some(row) = marker_row(&t.term, marker)
        {
            return screen::lines_text(&t.term, row + 1, t.term.screen_lines() as i32);
        }
        self.text(None)
    }
}

/// The last screen row of the line that reads `marker`, if one shows.
fn marker_row<T>(term: &Term<T>, marker: &str) -> Option<i32> {
    let grid = term.grid();
    let last = grid.columns().checked_sub(1)?;
    let mut line = String::new();
    let mut found = None;
    for row in 0..grid.screen_lines() as i32 {
        line.push_str(&screen::row_chars(term, Line(row)).0);
        // A narrow pane wraps the marker.
        if grid[Line(row)][Column(last)].flags.contains(Flags::WRAPLINE) {
            continue;
        }
        if line.trim_end() == marker {
            found = Some(row);
        }
        line.clear();
    }
    found
}

/// The text of `term`'s primary grid, history first, up to its last line with something on it:
/// lines separated by CR LF, each starting and ending with default attributes, at most
/// `max_bytes` of the newest.
fn encode<T>(term: &Term<T>, max_bytes: usize) -> Vec<u8> {
    encode_rows(term, max_bytes).0
}

/// [`encode`], and the screen row of the last line it holds (below 0: a line of the history).
pub(super) fn encode_rows<T>(term: &Term<T>, max_bytes: usize) -> (Vec<u8>, i32) {
    let grid = term.grid();
    let cols = grid.columns();
    let top = -(grid.history_size() as i32);
    let mut bottom = grid.screen_lines() as i32 - 1;
    while bottom >= top && matches!(visible(&grid[Line(bottom)][..]), (0, None)) {
        bottom -= 1;
    }
    let wraps = |line: i32| grid[Line(line)][Column(cols - 1)].flags.contains(Flags::WRAPLINE);
    let mut lines = Vec::new();
    let mut size = 0;
    let mut end = bottom;
    while end >= top {
        let mut start = end;
        while start > top && wraps(start - 1) && end - start < MAX_WRAPPED_ROWS - 1 {
            start -= 1;
        }
        let rows: Vec<&[Cell]> = (start..=end).map(|l| &grid[Line(l)][..]).collect();
        let line = encode_line(&rows);
        size += line.len() + 2;
        if size > max_bytes {
            break;
        }
        lines.push(line);
        end = start - 1;
    }
    lines.reverse();
    (lines.join(&b"\r\n"[..]), bottom)
}

/// The attributes `cell` is drawn with, as one SGR sequence.
pub(super) fn sgr_of(cell: &Cell) -> Vec<u8> {
    let mut out = Vec::new();
    Pen::of(cell).write(&mut out);
    out
}

/// One line of text, over the rows it wraps on: all of each row but the last, which ends at its
/// last cell with something in it.
fn encode_line(rows: &[&[Cell]]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pen = Pen::plain();
    let mut link: Option<Hyperlink> = None;
    for (i, row) in rows.iter().enumerate() {
        let (count, fill) = if i + 1 < rows.len() { (row.len(), None) } else { visible(row) };
        for cell in &row[..count] {
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            // An image's cells are saved blank: the image is not.
            let image = cell.c == crate::graphics::PLACEHOLDER;
            let next = cell.hyperlink().filter(|h| !image && linkable(h.uri()));
            if next != link {
                out.extend_from_slice(b"\x1b]8;;");
                if let Some(h) = &next {
                    out.extend_from_slice(h.uri().as_bytes());
                }
                out.extend_from_slice(b"\x1b\\");
                link = next;
            }
            let p = if image { Pen::plain() } else { Pen::of(cell) };
            if p != pen {
                p.write(&mut out);
                pen = p;
            }
            push_char(&mut out, if image || cell.c == '\t' || cell.c.is_control() { ' ' } else { cell.c });
            for z in cell.zerowidth().filter(|_| !image).unwrap_or_default() {
                push_char(&mut out, *z);
            }
        }
        // A row filled to its edge with a colour: an erase fills it again, whatever the width.
        if let Some(bg) = fill {
            if link.take().is_some() {
                out.extend_from_slice(b"\x1b]8;;\x1b\\");
            }
            let p = Pen { bg, ..Pen::plain() };
            if p != pen {
                p.write(&mut out);
                pen = p;
            }
            out.extend_from_slice(b"\x1b[K");
        }
    }
    if link.is_some() {
        out.extend_from_slice(b"\x1b]8;;\x1b\\");
    }
    if pen != Pen::plain() {
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

/// How many cells of the last row of a line are saved, and the colour its end is filled with,
/// when blanks of one colour run to its edge.
fn visible(row: &[Cell]) -> (usize, Option<Color>) {
    let blank = |c: &Cell| {
        matches!(c.c, ' ' | '\t' | '\0')
            && !c.flags.intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
            && c.zerowidth().is_none()
    };
    let default_bg = Color::Named(NamedColor::Background);
    let end = row.iter().rposition(|c| !blank(c) || c.bg != default_bg).map_or(0, |i| i + 1);
    if end < row.len() {
        return (end, None);
    }
    // Blanks of one colour up to the edge: only their text is written, and an erase.
    let text = row.iter().rposition(|c| !blank(c)).map_or(0, |i| i + 1);
    match row.get(text) {
        Some(first) if first.bg != default_bg && row[text..].iter().all(|c| c.bg == first.bg) => (text, Some(first.bg)),
        _ => (end, None),
    }
}

fn push_char(out: &mut Vec<u8>, c: char) {
    if c.is_ascii() {
        out.push(c as u8);
    } else {
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
}

fn linkable(uri: &str) -> bool {
    !uri.is_empty() && uri.len() <= MAX_LINK && uri.bytes().all(|b| (0x21..0x7f).contains(&b))
}

/// The attributes a cell is drawn with, as far as saving goes.
#[derive(Clone, Copy, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

impl Pen {
    fn plain() -> Pen {
        Pen {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            underline: None,
        }
    }

    fn of(cell: &Cell) -> Pen {
        Pen { fg: cell.fg, bg: cell.bg, flags: cell.flags & STYLE, underline: cell.underline_color() }
    }

    /// Every attribute from scratch: a reset, then what is set.
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        for (flag, param) in ATTRIBUTES {
            if self.flags.contains(flag) {
                out.push(b';');
                out.extend_from_slice(param.as_bytes());
            }
        }
        color(out, self.fg, 30, 90, 38);
        color(out, self.bg, 40, 100, 48);
        if let Some(u) = self.underline {
            color(out, u, 0, 0, 58);
        }
        out.push(b'm');
    }
}

/// The SGR parameters of colour `c`: `base` + 0-7 for the first eight named colours, `bright` +
/// 0-7 for the next eight (with `extended`;5 when both are 0), `extended` for the others. The
/// default colours have none.
fn color(out: &mut Vec<u8>, c: Color, base: u8, bright: u8, extended: u8) {
    let params: &[u8] = match c {
        Color::Named(n) => {
            let i = match n as usize {
                i @ 0..=15 => i as u8,
                // Dim colours are made when drawing; one in the grid is the colour it dims.
                i @ 259..=266 => (i - 259) as u8,
                _ => return,
            };
            match (base, i) {
                (0, _) => &[extended, 5, i],
                (_, 0..=7) => &[base + i],
                _ => &[bright + i - 8],
            }
        }
        Color::Indexed(i) => &[extended, 5, i],
        Color::Spec(rgb) => &[extended, 2, rgb.r, rgb.g, rgb.b],
    };
    for p in params {
        out.push(b';');
        push_number(out, *p);
    }
}

fn push_number(out: &mut Vec<u8>, n: u8) {
    if n >= 100 {
        out.push(b'0' + n / 100);
    }
    if n >= 10 {
        out.push(b'0' + n / 10 % 10);
    }
    out.push(b'0' + n % 10);
}

/// The last `lines` lines of saved text, as an emulator may be given them: text, line ends,
/// attributes (SGR), erasing to the end of the line and links (OSC 8). Whatever else the file
/// holds is dropped: a query would make the emulator answer the new program, and a mode would
/// change how that one's output is read.
pub(super) fn clean(saved: &[u8], lines: usize) -> Vec<u8> {
    if lines == 0 {
        return Vec::new();
    }
    // Each line starts with default attributes: the last ones read alone.
    let from = if lines == usize::MAX {
        0
    } else {
        saved.iter().enumerate().rev().filter(|(_, b)| **b == b'\n').nth(lines - 1).map_or(0, |(i, _)| i + 1)
    };
    let mut kept = Clean(Vec::with_capacity(saved.len() - from));
    Parser::new().advance(&mut kept, &saved[from..]);
    kept.0
}

/// What [`clean`] keeps, as it reads it.
struct Clean(Vec<u8>);

impl Perform for Clean {
    fn print(&mut self, c: char) {
        if !c.is_control() && c != crate::graphics::PLACEHOLDER {
            push_char(&mut self.0, c);
        }
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\r' | b'\n') {
            self.0.push(byte);
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match action {
            'm' => {
                let groups: Vec<String> =
                    params.iter().map(|p| p.iter().map(u16::to_string).collect::<Vec<_>>().join(":")).collect();
                self.0.extend_from_slice(format!("\x1b[{}m", groups.join(";")).as_bytes());
            }
            'K' if params.iter().all(|p| p == [0]) => self.0.extend_from_slice(b"\x1b[K"),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let [kind, rest @ ..] = params else { return };
        // A link's parameters, then its address.
        let link = rest.join(&b';');
        let printable = link.iter().all(|b| (0x20..0x7f).contains(b));
        if *kind == b"8" && !rest.is_empty() && printable && link.len() <= 2 * MAX_LINK {
            self.0.extend_from_slice(b"\x1b]8;");
            self.0.extend_from_slice(&link);
            self.0.extend_from_slice(b"\x1b\\");
        }
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::term::Config as TermConfig;
    use alacritty_terminal::vte::ansi::{Processor, Rgb};

    use super::super::pty::{Listener, Size};
    use super::*;

    fn term(cols: u16, rows: u16, history: usize) -> Term<Listener> {
        let config = TermConfig { scrolling_history: history, ..TermConfig::default() };
        Term::new(config, &Size { cols, rows }, Listener::default())
    }

    fn feed(term: &mut Term<Listener>, bytes: &[u8]) {
        let mut parser: Processor = Processor::new();
        parser.advance(term, bytes);
    }

    /// Every line of the grid, history first, as text.
    fn all_text(term: &Term<Listener>) -> String {
        let top = -(term.grid().history_size() as i32);
        screen::lines_text(term, top, term.screen_lines() as i32)
    }

    /// The grid's text with the rows the emulator wrapped joined back.
    fn logical_lines(term: &Term<Listener>) -> String {
        let grid = term.grid();
        let mut out = String::new();
        for l in -(grid.history_size() as i32)..term.screen_lines() as i32 {
            let row = &grid[Line(l)][..];
            let spacers = Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
            out.extend(row.iter().filter(|c| !c.flags.intersects(spacers)).map(|c| c.c));
            if !row[row.len() - 1].flags.contains(Flags::WRAPLINE) {
                let trimmed = out.trim_end().len();
                out.truncate(trimmed);
                out.push('\n');
            }
        }
        out.trim_start_matches('\n').to_string()
    }

    fn cell_at<'a>(term: &'a Term<Listener>, text: &str) -> &'a Cell {
        let top = -(term.grid().history_size() as i32);
        for l in top..term.screen_lines() as i32 {
            let (line, index) = screen::row_chars(term, Line(l));
            if let Some(at) = line.find(text) {
                let chars = line[..at].chars().count();
                let col = index.iter().position(|i| *i == chars).unwrap();
                return &term.grid()[Line(l)][Column(col)];
            }
        }
        panic!("{text:?} is not in:\n{}", all_text(term));
    }

    #[test]
    fn text_and_attributes_come_back_at_another_width() {
        let mut before = term(20, 4, 100);
        feed(
            &mut before,
            "\x1b[1;31mred\x1b[0m plain \x1b[38;2;1;2;3mrgb\x1b[0m \x1b[4:3;58;5;9mcurl\x1b[0m\r\n\
             日本語の文字列はここで折り返されます end\r\n\
             \x1b]8;;https://example.org\x1b\\link\x1b]8;;\x1b\\ after\r\n\
             \x1b[44mbar\x1b[K\x1b[0m\r\n\
             \x1b[7minverse\x1b[0m\r\nlast"
                .as_bytes(),
        );
        let saved = encode(&before, MAX_SAVED_BYTES);
        let text = String::from_utf8(saved.clone()).unwrap();
        let mut after = term(13, 3, 100);
        feed(&mut after, &clean(&saved, 100));
        let expected = "red plain rgb curl\n日本語の文字列はここで折り返されます end\nlink after\nbar\ninverse\nlast\n";
        assert_eq!(logical_lines(&after), expected, "{text:?}");
        let red = cell_at(&after, "red");
        assert_eq!(red.fg, Color::Named(NamedColor::Red));
        assert!(red.flags.contains(Flags::BOLD));
        assert_eq!(cell_at(&after, "rgb").fg, Color::Spec(Rgb { r: 1, g: 2, b: 3 }));
        let curl = cell_at(&after, "curl");
        assert!(curl.flags.contains(Flags::UNDERCURL));
        assert_eq!(curl.underline_color(), Some(Color::Indexed(9)));
        assert!(!cell_at(&after, "plain").flags.contains(Flags::BOLD));
        assert_eq!(
            cell_at(&after, "link").hyperlink().map(|h| h.uri().to_string()).as_deref(),
            Some("https://example.org")
        );
        assert!(cell_at(&after, "after").hyperlink().is_none());
        assert!(cell_at(&after, "inverse").flags.contains(Flags::INVERSE));
        // The colour of the bar fills its row to the edge, at the new width too.
        let top = -(after.grid().history_size() as i32);
        let row = (top..3).find(|l| screen::row_text(&after, Line(*l)) == "bar").unwrap();
        let blue = Color::Named(NamedColor::Blue);
        assert!(after.grid()[Line(row)][..].iter().all(|c| c.bg == blue));
        let next = &after.grid()[Line(row + 1)][..];
        assert!(
            next.iter().all(|c| c.bg == Color::Named(NamedColor::Background)),
            "the colour stops at the line's end"
        );
    }

    #[test]
    fn a_wide_character_that_did_not_fit_moves_with_its_line() {
        let mut before = term(5, 3, 10);
        // `日` cannot start in the last column: the emulator leaves a spacer there and wraps.
        feed(&mut before, "abcd日e".as_bytes());
        let saved = encode(&before, MAX_SAVED_BYTES);
        assert_eq!(String::from_utf8(saved.clone()).unwrap(), "abcd日e");
        let mut wide = term(10, 3, 10);
        feed(&mut wide, &clean(&saved, 10));
        assert_eq!(screen::row_text(&wide, Line(0)), "abcd日e");
    }

    #[test]
    fn blank_lines_at_the_end_are_not_saved() {
        let mut t = term(10, 5, 10);
        // Spaces in a colour show nothing either.
        feed(&mut t, b"one\r\n\r\ntwo\r\n\x1b[31m   \x1b[0m\r\n");
        assert_eq!(encode(&t, MAX_SAVED_BYTES), b"one\r\n\r\ntwo");
        assert!(encode(&term(10, 5, 10), MAX_SAVED_BYTES).is_empty());
    }

    #[test]
    fn the_newest_lines_are_kept_within_the_limits() {
        let mut t = term(10, 5, 1000);
        let lines: String = (1..=500).map(|i| format!("line {i}\r\n")).collect();
        feed(&mut t, lines.as_bytes());
        let saved = encode(&t, 100);
        assert!(saved.len() <= 100);
        let text = String::from_utf8(saved.clone()).unwrap();
        assert!(text.ends_with("line 499\r\nline 500"), "{text:?}");
        assert!(!text.contains("line 1\r"));
        // On restore, the lines that fit in the new pane's history.
        let all = encode(&t, MAX_SAVED_BYTES);
        assert_eq!(clean(&all, 2), b"line 499\r\nline 500");
        assert_eq!(clean(b"one", 2), b"one");
        assert!(clean(&all, 0).is_empty());
    }

    #[test]
    fn a_line_wrapped_over_many_rows_is_saved_whole_up_to_a_point() {
        let mut t = term(10, 5, 3000);
        feed(&mut t, "x".repeat(10 * 1500).as_bytes());
        let saved = encode(&t, MAX_SAVED_BYTES);
        let text = String::from_utf8(saved).unwrap();
        let parts: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].len(), 10 * MAX_WRAPPED_ROWS as usize);
    }

    #[test]
    fn only_text_attributes_and_links_are_given_back() {
        let hostile = b"ok\x1b]0;title\x07\x1b[6n\x1b]52;c;aGk=\x07\x1b[?1049h\x1b[2J\x1bP$qm\x1b\\\x07\x1b[1;31mred\
                        \x1b[K\x1b[2K\x1b]8;;https://x.org\x1b\\l\x1b]8;;\x1b\\\x1b[>1u\x1bc";
        let kept = clean(hostile, 10);
        assert_eq!(kept, b"ok\x1b[1;31mred\x1b[K\x1b]8;;https://x.org\x1b\\l\x1b]8;;\x1b\\");
        // The same text fed to an emulator asks for nothing and changes no mode.
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut t = Term::new(TermConfig::default(), &Size { cols: 20, rows: 3 }, Listener(events.clone()));
        feed(&mut t, &kept);
        assert!(events.borrow().is_empty());
        assert_eq!(*t.mode(), TermMode::default());
    }

    #[test]
    fn a_restored_pane_tells_its_old_text_from_the_new() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let spec = PaneSpec {
            command: vec!["sh".into(), "-c".into(), "echo new-$((1+1)); sleep 5".into()],
            ..Default::default()
        };
        let mut pane = Pane::spawn(1, None, spec, 30, 10, &env, &tx).unwrap();
        pane.restore_scrollback(b"\x1b[31mDo you want to proceed?\x1b[0m\r\nold", Some(0), 100);
        let start = Instant::now();
        while !pane.text(None).contains("new-2") {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "{}", pane.text(None));
            while let Some(data) = pane.take_output() {
                pane.feed(&data);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let screen = pane.text(None);
        assert!(screen.starts_with("Do you want to proceed?\nold\nkraal: restored from "), "{screen}");
        assert!(screen.contains("new-2"), "{screen}");
        assert_eq!(pane.live_text().trim(), "new-2");
        // The marker is dim, and the old text keeps its colour.
        let t = pane.term().unwrap();
        assert_eq!(t.term.grid()[Line(0)][Column(0)].fg, Color::Named(NamedColor::Red));
        assert!(t.term.grid()[Line(2)][Column(0)].flags.contains(Flags::DIM));
        // Saved again, it holds both, then the next restore marks where each ends.
        let saved = pane.scrollback_ansi().unwrap();
        let text = String::from_utf8(saved).unwrap();
        assert!(text.contains("old\r\n\x1b[0;2mkraal: restored from"), "{text:?}");
        assert!(text.ends_with("new-2"), "{text:?}");
        // A narrow pane wraps the marker: the old text is still told apart.
        pane.resize(12, 10);
        assert_eq!(pane.live_text().trim(), "new-2");
        pane.kill();
    }

    #[test]
    fn restored_text_is_searched_and_copied_and_new_commands_are_marked() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let spec = PaneSpec { command: vec!["sleep".into(), "5".into()], ..Default::default() };
        let mut pane = Pane::spawn(1, None, spec, 20, 4, &env, &tx).unwrap();
        let old: Vec<String> = (1..=20).map(|i| format!("old line {i}")).collect();
        assert!(pane.restore_scrollback(old.join("\r\n").as_bytes(), Some(0), 1000));
        assert!(pane.search("old line 3"));
        assert_eq!(pane.selection_text().as_deref(), Some("old line 3"));
        pane.end_search();
        pane.clear_selection();
        pane.copy_mode(true);
        pane.vi_edge(true);
        let t = pane.term().unwrap();
        assert_eq!(screen::row_text(&t.term, t.term.vi_mode_cursor.point.line), "old line 1");
        pane.copy_mode(false);
        // The shell's marks after the restore place its commands.
        pane.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07file1\r\n\x1b]133;D;0\x07");
        assert_eq!(pane.last_command_output(), Some((Some("ls".into()), "file1".into())));
        pane.kill();
    }

    #[test]
    fn nothing_is_saved_from_the_alternate_screen() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let spec = PaneSpec { command: vec!["sleep".into(), "5".into()], ..Default::default() };
        let mut pane = Pane::spawn(1, None, spec, 20, 4, &env, &tx).unwrap();
        pane.feed(b"shell text\r\n");
        assert_eq!(pane.scrollback_ansi().as_deref(), Some(&b"shell text"[..]));
        pane.feed(b"\x1b[?1049hvim");
        assert_eq!(pane.scrollback_ansi(), None);
        pane.feed(b"\x1b[?1049l");
        assert_eq!(pane.scrollback_ansi().as_deref(), Some(&b"shell text"[..]));
        pane.kill();
    }
}
