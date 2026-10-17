use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color as AColor, Processor, Rgb};
use base64::Engine;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use super::scan::{MAX_APC, Scanner};
use super::*;

struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}

fn term(cols: usize, lines: usize) -> Term<VoidListener> {
    Term::new(Config::default(), &Size(cols, lines), VoidListener)
}

fn shared(available: bool) -> Arc<Shared> {
    let s = Arc::new(Shared::new(true, 64 << 20));
    s.set_available(available);
    s.set_cell(10, 20);
    s
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// A kitty command for a `w × h` image of one colour, sent as RGB.
fn rgb_image(keys: &str, w: u32, h: u32) -> Vec<u8> {
    let data: Vec<u8> = (0..w * h).flat_map(|_| [200u8, 10, 10]).collect();
    format!("\x1b_G{keys},f=24,s={w},v={h};{}\x1b\\", b64(&data)).into_bytes()
}

/// Feed `bytes` as a pane does: text to the emulator, images to `g`; the replies given at once.
fn feed(g: &mut PaneGraphics, t: &mut Term<VoidListener>, bytes: &[u8]) -> Vec<u8> {
    let mut parser: Processor = Processor::new();
    let mut replies = Vec::new();
    for piece in g.split(bytes) {
        match piece {
            Piece::Text(text) => parser.advance(t, &text),
            image => {
                if let Answer::Now(Some(reply)) = g.handle(image, t) {
                    replies.extend(reply);
                }
            }
        }
    }
    replies
}

/// Hand `bytes`, images only, to `g`: what the program is told of each.
fn answers(g: &mut PaneGraphics, t: &mut Term<VoidListener>, bytes: &[u8]) -> Vec<Answer> {
    g.split(bytes).into_iter().map(|piece| g.handle(piece, t)).collect()
}

/// Prepare the pictures that wait, as the server does off its loop; the replies they owed.
fn prepare_all(g: &mut PaneGraphics) -> Vec<u8> {
    let mut replies = Vec::new();
    while let Some(work) = g.next_work() {
        if let Some(Owed(Some(reply))) = g.prepared(work()) {
            replies.extend(reply);
        }
    }
    replies
}

/// A picture prepared from `rgba`, counted in `shared`.
fn ready_picture(width: u32, height: u32, rgba: &[u8], shared: &Arc<Shared>) -> Arc<Picture> {
    let picture = Picture::pending((width, height), shared);
    picture.fill(encode_png(width, height, rgba).unwrap());
    picture
}

fn cell_at(t: &Term<VoidListener>, line: i32, col: usize) -> Option<Placeholder> {
    Placeholder::of_cell(&t.grid()[Line(line)][Column(col)])
}

/// The screen as a client sees it: the images as cells its terminal draws when it `show`s them.
fn render(g: &PaneGraphics, t: &Term<VoidListener>, show: bool) -> Buffer {
    let area = Rect::new(0, 0, t.columns() as u16, t.screen_lines() as u16);
    let mut buf = Buffer::empty(area);
    crate::pane::render_term(t, area, &mut buf, None);
    g.show_cells(area, &mut buf, show);
    buf
}

#[test]
fn image_sequences_are_taken_out_wherever_reads_cut_them() {
    let stream: &[u8] = b"a\x1b_Gf=100,a=T;QUJD\x1b\\b\x1bP0;1q#0~~-\x1b\\c\x1b[1md\x1bPzz\x1b\\e";
    for size in 1..stream.len() {
        let mut s = Scanner::default();
        let mut pieces = Vec::new();
        for chunk in stream.chunks(size) {
            pieces.extend(s.split(chunk));
        }
        // Text pieces cut by the reads come back together.
        let mut merged: Vec<Piece> = Vec::new();
        for p in pieces {
            match (merged.last_mut(), p) {
                (Some(Piece::Text(a)), Piece::Text(b)) => a.to_mut().extend_from_slice(&b),
                (_, p) => merged.push(p),
            }
        }
        assert_eq!(
            merged,
            vec![
                Piece::Text(b"a".to_vec().into()),
                Piece::Kitty(b"f=100,a=T;QUJD".to_vec()),
                Piece::Text(b"b".to_vec().into()),
                // Measured as it came, whatever the cuts.
                Piece::Sixel { params: b"0;1".to_vec(), data: b"#0~~-".to_vec(), size: Some((2, 6)) },
                // A DCS that is not sixel, and the rest, go to the emulator as they came.
                Piece::Text(b"c\x1b[1md\x1bPzz\x1b\\e".to_vec().into()),
            ],
            "chunks of {size}"
        );
    }
}

#[test]
fn reads_without_images_go_on_whole() {
    let mut s = Scanner::default();
    let pieces = s.split(b"plain \x1b[1mtext\x1b");
    assert!(matches!(&pieces[..], [Piece::Text(std::borrow::Cow::Borrowed(_))]));
    assert_eq!(pieces, vec![Piece::Text(b"plain \x1b[1mtext".to_vec().into())]);
    // The ESC the read ended with was held: it starts no image, so it goes on first.
    let pieces = s.split(b"[0m\x1b\x1b[1m");
    assert_eq!(pieces, vec![Piece::Text(b"\x1b".to_vec().into()), Piece::Text(b"[0m\x1b\x1b[1m".to_vec().into())]);
    // Reads with images are split as before.
    assert_eq!(s.split(b"a\x1b_Gq=2;\x1b\\b").len(), 3);
}

#[test]
fn a_cancelled_or_oversized_command_is_dropped() {
    let mut s = Scanner::default();
    assert_eq!(s.split(b"x\x1b_Ga=T\x18y"), vec![Piece::Text(b"xy".to_vec().into())]);
    let mut s = Scanner::default();
    let mut huge = b"\x1b_Ga=T;".to_vec();
    huge.resize(MAX_APC + 100, b'A');
    huge.extend_from_slice(b"\x1b\\z");
    assert_eq!(s.split(&huge), vec![Piece::Text(b"z".to_vec().into())]);
    // An ESC inside a command starts a new sequence.
    let mut s = Scanner::default();
    assert_eq!(s.split(b"\x1b_Ga=T\x1b[1m"), vec![Piece::Text(b"\x1b[1m".to_vec().into())]);
}

#[test]
fn every_diacritic_is_a_zero_width_mark() {
    use unicode_width::UnicodeWidthChar;
    assert_eq!(PLACEHOLDER.width(), Some(1));
    for d in DIACRITICS {
        assert_eq!(d.width(), Some(0), "{d:?}");
    }
}

#[test]
fn a_placed_image_becomes_placeholder_cells_at_the_cursor() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(20, 6);
    let replies = feed(&mut g, &mut t, b"ab");
    assert!(replies.is_empty());
    // 3 columns by 2 rows, then text after it.
    let replies = feed(&mut g, &mut t, &rgb_image("a=T,i=5,c=3,r=2", 4, 4));
    assert_eq!(replies, b"\x1b_Gi=5;OK\x1b\\");
    let (kid, shown) = g.shown().next().unwrap();
    assert_eq!(kid >> 24, KRAAL_ID >> 24);
    assert_eq!(shown.placements, vec![(0, 3, 2)]);
    for (line, col, row, column) in [(0, 2, 0, 0), (0, 4, 0, 2), (1, 2, 1, 0), (1, 4, 1, 2)] {
        let p = cell_at(&t, line, col).unwrap();
        assert_eq!((p.id, p.row, p.col), (kid, row, column), "line {line} col {col}");
    }
    assert!(cell_at(&t, 0, 5).is_none() && cell_at(&t, 1, 1).is_none());
    // The cursor is after the image, on its last row; what follows keeps its own colours.
    assert_eq!(t.grid().cursor.point, alacritty_terminal::index::Point::new(Line(1), Column(5)));
    feed(&mut g, &mut t, b"z");
    assert_eq!(t.grid()[Line(1)][Column(5)].c, 'z');
    assert_ne!(
        t.grid()[Line(1)][Column(5)].fg,
        AColor::Spec(Rgb { r: (kid >> 16) as u8, g: (kid >> 8) as u8, b: kid as u8 })
    );
}

#[test]
fn an_image_wider_than_the_screen_is_cut_and_c1_keeps_the_cursor() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(10, 4);
    feed(&mut g, &mut t, b"\x1b[1;8H");
    feed(&mut g, &mut t, &rgb_image("a=T,C=1,c=6,r=2,q=1", 2, 2));
    let (_, shown) = g.shown().next().unwrap();
    assert_eq!(shown.placements, vec![(0, 3, 2)], "three columns fit");
    assert!(cell_at(&t, 1, 9).is_some());
    assert_eq!(t.grid().cursor.point, alacritty_terminal::index::Point::new(Line(0), Column(7)));
}

#[test]
fn queries_are_answered_only_when_a_terminal_can_show_images() {
    let query = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
    let mut t = term(10, 4);
    let mut g = PaneGraphics::new(shared(false));
    assert!(feed(&mut g, &mut t, query).is_empty(), "like a terminal without images");
    let mut g = PaneGraphics::new(shared(true));
    assert_eq!(feed(&mut g, &mut t, query), b"\x1b_Gi=31;OK\x1b\\");
    assert_eq!(g.shown().count(), 0, "a query keeps nothing");
    assert_eq!(g.device_attributes("\x1b[?6c"), "\x1b[?62;4;22c");
    assert_eq!(PaneGraphics::new(shared(false)).device_attributes("\x1b[?6c"), "\x1b[?6c");
}

#[test]
fn files_and_shared_memory_are_refused_alike() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(10, 4);
    for medium in ["f", "t", "s"] {
        let cmd = format!("\x1b_Gi=3,a=T,f=100,t={medium};{}\x1b\\", b64(b"/etc/passwd"));
        assert_eq!(feed(&mut g, &mut t, cmd.as_bytes()), b"\x1b_Gi=3;EBADF:Failed to read image file\x1b\\");
    }
    assert_eq!(g.shown().count(), 0);
}

#[test]
fn chunks_numbers_and_quiet_replies() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(20, 6);
    let data: Vec<u8> = (0..4 * 4).flat_map(|_| [1u8, 2, 3, 255]).collect();
    let all = b64(&data);
    let (a, b) = all.split_at(32);
    let first = format!("\x1b_GI=9,f=32,s=4,v=4,m=1;{a}\x1b\\");
    let last = format!("\x1b_Gm=0;{b}\x1b\\");
    assert!(feed(&mut g, &mut t, first.as_bytes()).is_empty());
    let reply = String::from_utf8(feed(&mut g, &mut t, last.as_bytes())).unwrap();
    // A numbered image gets an id of kraal's, given back with the number.
    let id: u32 = reply.strip_prefix("\x1b_Gi=").unwrap().split(',').next().unwrap().parse().unwrap();
    assert_eq!(reply, format!("\x1b_Gi={id},I=9;OK\x1b\\"));
    // Placed by its number; q=1 silences the OK, q=2 errors too.
    assert!(feed(&mut g, &mut t, b"\x1b_Ga=p,I=9,q=1\x1b\\").is_empty());
    assert_eq!(g.shown().count(), 1);
    assert!(feed(&mut g, &mut t, b"\x1b_Ga=p,i=77,q=2\x1b\\").is_empty());
    assert_eq!(feed(&mut g, &mut t, b"\x1b_Ga=p,i=77\x1b\\"), b"\x1b_Gi=77;ENOENT:no such image\x1b\\");
}

#[test]
fn deleted_images_render_blank_and_others_show() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(20, 6);
    feed(&mut g, &mut t, &rgb_image("a=T,i=1,c=2,r=1,q=2", 2, 2));
    feed(&mut g, &mut t, b"\r\n");
    feed(&mut g, &mut t, &rgb_image("a=T,i=2,c=2,r=1,q=2", 2, 2));
    assert_eq!(g.shown().count(), 2);
    let buf = render(&g, &t, true);
    assert!(buf[(0, 0)].symbol().starts_with(PLACEHOLDER) && buf[(0, 1)].symbol().starts_with(PLACEHOLDER));
    assert!(matches!(buf[(0, 0)].fg, Color::Rgb(..)));
    // A client that cannot show images sees blanks, never the placeholder character.
    let buf = render(&g, &t, false);
    assert_eq!(buf[(0, 0)].symbol(), " ");
    feed(&mut g, &mut t, b"\x1b_Ga=d,d=I,i=1\x1b\\");
    assert_eq!(g.shown().count(), 1);
    let buf = render(&g, &t, true);
    assert_eq!(buf[(0, 0)].symbol(), " ");
    assert!(buf[(0, 1)].symbol().starts_with(PLACEHOLDER));
    // By position: the image under the cursor's cell (1-based x, y).
    feed(&mut g, &mut t, b"\x1b_Ga=d,d=p,x=1,y=2\x1b\\");
    assert_eq!(g.shown().count(), 0);
}

#[test]
fn a_programs_own_placeholders_name_its_image_through_kraal() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(20, 6);
    // A virtual placement, then the program prints placeholders for id 42 (256-colour), with
    // the row diacritic only on the first cell, as kitty allows.
    feed(&mut g, &mut t, &rgb_image("a=T,U=1,i=42,c=3,r=1,q=2", 3, 1));
    let (kid, shown) = g.shown().next().unwrap();
    assert_eq!(shown.placements, vec![(0, 3, 1)]);
    assert!(t.grid()[Line(0)][Column(0)].c == ' ', "a virtual placement writes nothing");
    let text = format!("\x1b[38;5;42m{PLACEHOLDER}\u{0305}{PLACEHOLDER}{PLACEHOLDER}\x1b[39m");
    feed(&mut g, &mut t, text.as_bytes());
    let buf = render(&g, &t, true);
    for col in 0..3u16 {
        let p = Placeholder::of_symbol(buf[(col, 0)].symbol(), buf[(col, 0)].fg).unwrap();
        assert_eq!((p.id, p.row, p.col), (kid, 0, col as usize), "column {col}");
    }
    // An id nothing shows renders blank.
    let text = format!("\r\n\x1b[38;5;43m{PLACEHOLDER}\x1b[39m");
    feed(&mut g, &mut t, text.as_bytes());
    let buf = render(&g, &t, true);
    assert_eq!(buf[(0, 1)].symbol(), " ");
}

#[test]
fn the_oldest_images_go_when_the_quota_is_reached() {
    let s = Arc::new(Shared::new(true, 1));
    s.set_available(true);
    let mut g = PaneGraphics::new(s.clone());
    let mut t = term(20, 6);
    feed(&mut g, &mut t, &rgb_image("a=T,i=1,q=2", 2, 2));
    // Even one image is over a quota of one byte: it cannot stay.
    assert_eq!(g.shown().count(), 0);
    assert_eq!(feed(&mut g, &mut t, &rgb_image("a=T,i=2", 2, 2)), b"\x1b_Gi=2;ENOSPC:image storage is full\x1b\\");
    assert_eq!(s.used(), 0, "nothing is left held");
    // With room for one, the second replaces the first.
    // What one picture counts: its PNG and the cost of keeping it.
    let one = {
        let probe = Arc::new(Shared::new(true, usize::MAX));
        let _picture = ready_picture(2, 2, &[200, 10, 10, 255].repeat(4), &probe);
        probe.used()
    };
    let s = Arc::new(Shared::new(true, one + one / 2));
    s.set_available(true);
    let mut g = PaneGraphics::new(s.clone());
    feed(&mut g, &mut t, &rgb_image("a=T,i=1,q=2", 2, 2));
    feed(&mut g, &mut t, &rgb_image("a=T,i=2,q=2", 2, 2));
    assert_eq!(g.shown().count(), 1);
    assert!(g.shown().all(|(_, sh)| sh.origin == 2));
    assert!(s.used() <= one + one / 2);
    drop(g);
    assert_eq!(s.used(), 0, "a pane's images go with it");
}

#[test]
fn sixel_images_decode_and_take_cells() {
    // Two red columns six pixels high, then a band below in register 1's default blue.
    let (size, pixels) = sixel::decode(b"\"1;1;2;12#0;2;100;0;0#0~~-#1!2~").unwrap();
    assert_eq!(size, (2, 12));
    assert_eq!(&pixels[..4], &[255, 0, 0, 255]);
    assert_eq!(&pixels[6 * 2 * 4..6 * 2 * 4 + 4], &[51, 51, 204, 255]);
    let (size, pixels) = sixel::decode(b"#0;2;0;100;0@").unwrap();
    assert_eq!(size, (1, 1));
    assert_eq!(&pixels[..], &[0, 255, 0, 255]);
    // Unpainted pixels are transparent; blank sixels and the size announced, even last, count.
    let (size, pixels) = sixel::decode(b"@!3?-?\"1;1;2;9").unwrap();
    assert_eq!(size, (4, 9));
    assert!(pixels[4..].iter().all(|p| *p == 0));
    assert!(sixel::decode(b"!99999999~").is_none(), "too wide");
    assert!(sixel::decode(b"").is_none());

    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(20, 6);
    feed(&mut g, &mut t, b"\x1bPq\"1;1;25;30#0;2;0;0;100#0!25~-!25~-!25~-!25~-!25~\x1b\\");
    let (_, shown) = g.shown().next().unwrap();
    // 25×30 pixels in 10×20 cells: 3 columns, 2 rows; the cursor goes below.
    assert_eq!(shown.placements, vec![(0, 3, 2)]);
    assert_eq!(t.grid().cursor.point.line, Line(2));
}

/// A sixel image of `width × height` pixels in one colour, too large to paint at once.
fn large_sixel(width: u32, height: u32) -> Vec<u8> {
    let mut image = format!("\x1bPq\"1;1;{width};{height}#1").into_bytes();
    for _ in 0..height.div_ceil(6) {
        image.extend_from_slice(format!("!{width}~-").as_bytes());
    }
    image.extend_from_slice(b"\x1b\\");
    image
}

#[test]
fn a_large_sixel_takes_its_cells_at_once_and_shows_once_painted() {
    let s = shared(true);
    let mut g = PaneGraphics::new(s.clone());
    let mut t = term(80, 40);
    // 600×300 pixels: 60×15 cells of 10×20, painted off the server's loop. What follows it goes
    // below it, where the cursor went.
    let image = large_sixel(600, 300);
    feed(&mut g, &mut t, &[&image[..], b"after"].concat());
    let kid = cell_at(&t, 0, 0).unwrap().id;
    assert_eq!(cell_at(&t, 14, 59).map(|p| (p.id, p.row, p.col)), Some((kid, 14, 59)));
    assert_eq!(t.grid()[Line(15)][Column(0)].c, 'a');
    // Meanwhile its cells render blank, and its data counts in the quota.
    assert_eq!(g.shown().count(), 0);
    assert_eq!(render(&g, &t, true)[(0, 0)].symbol(), " ");
    let data = image.len() - b"\x1bPq".len() - b"\x1b\\".len();
    assert_eq!(s.used(), data + PICTURE_COST);
    assert!(prepare_all(&mut g).is_empty(), "sixel has no reply");
    let (_, shown) = g.shown().next().unwrap();
    assert_eq!(s.used(), shown.picture.png().unwrap().len() + PICTURE_COST);
    assert!(render(&g, &t, true)[(0, 0)].symbol().starts_with(PLACEHOLDER));
}

#[test]
fn a_large_kitty_image_is_placed_at_once_and_answered_once_prepared() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(80, 40);
    assert_eq!(answers(&mut g, &mut t, &rgb_image("a=T,i=7,c=4,r=2", 300, 300)), [Answer::Later]);
    assert_eq!(prepare_all(&mut g), b"\x1b_Gi=7;OK\x1b\\");
    assert_eq!(g.shown().count(), 1);
    // Data that does not make the image it says: the program hears why, and nothing keeps it.
    let wrong = format!("\x1b_Ga=T,i=8,f=24,s=300,v=300;{}\x1b\\", b64(&[0; 30]));
    assert_eq!(answers(&mut g, &mut t, wrong.as_bytes()), [Answer::Later]);
    assert_eq!(prepare_all(&mut g), b"\x1b_Gi=8;ENODATA:size does not match the data\x1b\\");
    assert_eq!(g.shown().count(), 1);
    assert_eq!(feed(&mut g, &mut t, b"\x1b_Ga=p,i=8\x1b\\"), b"\x1b_Gi=8;ENOENT:no such image\x1b\\");
    // Both took their cells at once, side by side.
    let [first, second] = [cell_at(&t, 1, 3), cell_at(&t, 1, 4)].map(|p| p.map(|p| (p.row, p.col)));
    assert_eq!((first, second), (Some((1, 3)), Some((0, 0))));
}

#[test]
fn what_waits_to_be_prepared_counts_in_the_quota() {
    // Room for one and a half transmissions of an image.
    let image = |keys: &str| rgb_image(keys, 300, 300);
    let s = Arc::new(Shared::new(true, image("q=2").len() * 3 / 2));
    s.set_available(true);
    let mut t = term(80, 40);
    // The oldest image goes for a new one, with what was sent for it.
    let mut g = PaneGraphics::new(s.clone());
    feed(&mut g, &mut t, &image("a=T,i=1,q=2"));
    feed(&mut g, &mut t, &image("a=T,i=2,q=2"));
    assert_eq!(g.jobs.len(), 1);
    drop(g);
    // What was sent by a program waiting to hear how it went stays: no room for more.
    let mut g = PaneGraphics::new(s.clone());
    assert_eq!(answers(&mut g, &mut t, &image("a=t,i=1")), [Answer::Later]);
    assert_eq!(feed(&mut g, &mut t, &image("a=t,i=2")), b"\x1b_Gi=2;ENOSPC:image storage is full\x1b\\");
    assert_eq!(prepare_all(&mut g), b"\x1b_Gi=1;OK\x1b\\");
    drop(g);
    assert_eq!(s.used(), 0);
}

#[test]
fn unfinished_chunked_uploads_count_in_the_shared_quota() {
    let s = Arc::new(Shared::new(true, 1000));
    s.set_available(true);
    let mut t = term(20, 6);
    let first = format!("\x1b_Ga=t,i=1,f=32,s=100,v=100,m=1;{}\x1b\\", "A".repeat(400));
    let more = format!("\x1b_Gm=1;{}\x1b\\", "A".repeat(400));
    let mut panes: Vec<PaneGraphics> = (0..3).map(|_| PaneGraphics::new(s.clone())).collect();
    assert!(feed(&mut panes[0], &mut t, first.as_bytes()).is_empty());
    assert!(feed(&mut panes[0], &mut t, more.as_bytes()).is_empty());
    assert_eq!(s.used(), 800);
    // Other panes' uploads are refused past the quota, and what they sent goes.
    for g in &mut panes[1..] {
        assert_eq!(feed(g, &mut t, first.as_bytes()), b"\x1b_Gi=1;ENOSPC:image storage is full\x1b\\");
        feed(g, &mut t, more.as_bytes());
        assert_eq!(s.used(), 800);
    }
    // A deletion, or the pane closing, ends an upload and frees what it held.
    feed(&mut panes[0], &mut t, b"\x1b_Ga=d\x1b\\");
    assert_eq!(s.used(), 0);
    feed(&mut panes[1], &mut t, first.as_bytes());
    assert_eq!(s.used(), 400);
    drop(panes);
    assert_eq!(s.used(), 0);
}

#[test]
fn a_part_of_a_picture_being_prepared_is_cut_once_it_is() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(80, 40);
    feed(&mut g, &mut t, &rgb_image("a=t,i=1,q=2", 300, 300));
    assert_eq!(feed(&mut g, &mut t, b"\x1b_Ga=p,i=1,w=10,h=10\x1b\\"), b"\x1b_Gi=1;OK\x1b\\");
    assert!(prepare_all(&mut g).is_empty());
    let (_, shown) = g.shown().next().unwrap();
    assert_eq!((shown.picture.width, shown.picture.height), (10, 10));
    assert_eq!(shown.placements, vec![(0, 1, 1)]);
}

#[test]
fn pictures_nothing_waits_for_are_not_prepared() {
    let s = shared(true);
    let mut g = PaneGraphics::new(s.clone());
    let mut t = term(80, 40);
    // Deleted before its turn: its data counts no more.
    feed(&mut g, &mut t, &large_sixel(600, 300));
    feed(&mut g, &mut t, b"\x1b_Ga=d\x1b\\");
    assert!(g.next_work().is_none());
    assert_eq!(s.used(), 0);
    // A file manager's previews, one over the other on the alternate screen: only the last waits.
    feed(&mut g, &mut t, b"\x1b[?1049h");
    for _ in 0..3 {
        feed(&mut g, &mut t, b"\x1b[2J\x1b[H");
        feed(&mut g, &mut t, &large_sixel(600, 300));
    }
    assert_eq!(g.jobs.len(), 1);
    // Its pane closed while it is painted: no PNG is made.
    let work = g.next_work().unwrap();
    drop(g);
    assert!(work().is_err());
    assert_eq!(s.used(), 0);
}

#[test]
fn outer_terminal_commands_are_plain_and_chunked() {
    let mut out = Vec::new();
    transmit(KRAAL_ID | 5, &[7u8; 10_000], &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with(&format!("\x1b_Ga=t,f=100,i={},q=2,m=1;", KRAAL_ID | 5)));
    assert!(text.ends_with("\x1b\\"));
    assert_eq!(text.matches("\x1b_G").count(), 10_000_usize.div_ceil(3) * 4 / 4096 + 1);
    let mut out = Vec::new();
    place(9, 3, 4, 2, &mut out);
    assert_eq!(out, b"\x1b_Ga=p,U=1,i=9,p=3,c=4,r=2,q=2\x1b\\");
    assert_eq!(probe_answer(&format!("\x1b_Gi={PROBE_ID};OK\x1b\\")), Some(true));
    assert_eq!(probe_answer(&format!("\x1b_Gi={PROBE_ID};ENOTSUPPORTED\x1b\\")), Some(false));
    assert_eq!(probe_answer("\x1b[?62c"), None);
    assert_eq!(cell_size_answer("\x1b[6;20;9t"), Some((9, 20)));
    assert_eq!(terminal_name("\x1bP>|ghostty 1.1.3\x1b\\").as_deref(), Some("ghostty 1.1.3"));
    assert!(draws_placeholders("kitty(0.39.1)") && draws_placeholders("ghostty 1.1.3"));
    assert!(!draws_placeholders("WezTerm 20240203-110809-5046fc22"));
}

#[test]
fn terminal_replies_reach_the_server_whole() {
    let mut p = crate::input::InputParser::new();
    let mut events = p.feed(format!("\x1b_Gi={PROBE_ID};OK\x1b\\\x1bP>|kitty(0.39.1)\x1b\\\x1b[6;20;9t").as_bytes());
    events.extend(p.flush());
    let raw: Vec<String> = events
        .into_iter()
        .filter_map(|e| match e {
            crate::input::InputEvent::Raw(r) => Some(String::from_utf8_lossy(&r).into_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(
        raw,
        vec![
            format!("\x1b_Gi={PROBE_ID};OK\x1b\\"),
            "\x1bP>|kitty(0.39.1)\x1b\\".to_string(),
            "\x1b[6;20;9t".to_string()
        ]
    );
}

/// A quota that holds `n` pictures of `w × h` sent as RGB.
fn quota_for(n: usize, w: u32, h: u32) -> Arc<Shared> {
    let probe = Arc::new(Shared::new(true, usize::MAX));
    let _picture = ready_picture(w, h, &[200, 10, 10, 255].repeat((w * h) as usize), &probe);
    let s = Arc::new(Shared::new(true, probe.used() * n + probe.used() / 2));
    s.set_available(true);
    s.set_cell(10, 20);
    s
}

/// Each placement of a part of a picture decodes it whole: a few bytes of output that ask for
/// many parts of a large picture get decoded no more pixels a second than the pane's budget.
#[test]
fn parts_of_a_large_picture_are_decoded_at_a_bounded_rate() {
    let s = shared(true);
    let mut t = term(80, 24);
    let mut g = PaneGraphics::new(s.clone());
    g.budget.limit = 250_000;
    feed(&mut g, &mut t, &rgb_image("a=t,i=7", 500, 500));
    let part = |g: &mut PaneGraphics, t: &mut Term<VoidListener>, x: u32| {
        let reply = feed(g, t, format!("\x1b_Ga=p,i=7,x={x},y=1,w=10,h=10\x1b\\").as_bytes());
        String::from_utf8_lossy(&reply).contains("EBUSY")
    };
    // The picture took the second's pixels.
    assert!((0..20).all(|x| part(&mut g, &mut t, x)));
    // A second later, one part fits, not the next.
    g.budget.since -= std::time::Duration::from_secs(1);
    assert!(!part(&mut g, &mut t, 3));
    assert!(part(&mut g, &mut t, 4));
    // Sixel images wait too.
    let before = g.shown.len();
    feed(&mut g, &mut t, b"\x1bPq\"1;1;400;400#1~~~\x1b\\");
    assert_eq!(g.shown.len(), before);
}

#[test]
fn cropped_placements_count_in_the_quota() {
    // Room for three pictures: the upload, and two parts of it at most.
    let s = quota_for(3, 8, 8);
    let mut g = PaneGraphics::new(s.clone());
    let mut t = term(40, 10);
    feed(&mut g, &mut t, &rgb_image("a=t,i=1,q=2", 8, 8));
    let quota = s.used() * 3 + s.used() / 2;
    for x in 0..20 {
        feed(&mut g, &mut t, format!("\x1b_Ga=p,i=1,x={},w=4,C=1,q=2\x1b\\", x % 4).as_bytes());
        assert!(s.used() <= quota, "placement {x}: {} over {quota}", s.used());
    }
    assert!(g.shown().count() <= 3);
}

#[test]
fn placements_are_bounded_and_those_written_over_are_forgotten() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(20, 4);
    feed(&mut g, &mut t, &rgb_image("a=t,i=1,q=2", 2, 2));
    // The same cell, placed over and over: what was written over goes.
    for _ in 0..200 {
        feed(&mut g, &mut t, b"\x1b[H\x1b_Ga=p,i=1,c=1,r=1,C=1,q=2\x1b\\");
    }
    assert!(g.shown().count() <= 2 * SWEEP_START, "{} images kept", g.shown().count());
    // One per line: those still in the screen or its history stay, the others go.
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(20, 4);
    feed(&mut g, &mut t, &rgb_image("a=t,i=1,q=2", 2, 2));
    for _ in 0..2 * SWEEP_START {
        feed(&mut g, &mut t, b"\x1b_Ga=p,i=1,c=1,r=1,q=2\x1b\\\r\n");
    }
    assert_eq!(g.shown().count(), 2 * SWEEP_START, "all in the history");
    let mut short = Term::new(Config { scrolling_history: 2, ..Config::default() }, &Size(20, 4), VoidListener);
    let mut g = PaneGraphics::new(shared(true));
    feed(&mut g, &mut short, &rgb_image("a=t,i=1,q=2", 2, 2));
    for _ in 0..2 * SWEEP_START {
        feed(&mut g, &mut short, b"\x1b_Ga=p,i=1,c=1,r=1,q=2\x1b\\\r\n");
    }
    assert!(g.shown().count() < SWEEP_START, "{} kept with a history of 2 lines", g.shown().count());
    // A program's own placeholders: placements of one image are bounded.
    let mut g = PaneGraphics::new(shared(true));
    feed(&mut g, &mut t, &rgb_image("a=t,i=7,q=2", 2, 2));
    for p in 1..=200 {
        feed(&mut g, &mut t, format!("\x1b_Ga=p,U=1,i=7,p={p},c=1,r=1,q=2\x1b\\").as_bytes());
    }
    let (_, shown) = g.shown().next().unwrap();
    assert_eq!(shown.placements.len(), MAX_PLACEMENTS);
    assert_eq!(shown.placements.last().map(|p| p.0), Some(200), "the newest stay");
}

#[test]
fn the_alternate_screen_keeps_the_main_screens_images() {
    let mut g = PaneGraphics::new(shared(true));
    let mut t = term(20, 4);
    feed(&mut g, &mut t, &rgb_image("a=t,i=1,q=2", 2, 2));
    feed(&mut g, &mut t, b"\x1b_Ga=p,i=1,c=1,r=1,q=2\x1b\\");
    let main: Vec<u32> = g.shown().map(|(k, _)| k).collect();
    // On the alternate screen, many images over the same cell: the main screen's stays.
    feed(&mut g, &mut t, b"\x1b[?1049h");
    for _ in 0..100 {
        feed(&mut g, &mut t, b"\x1b[H\x1b_Ga=p,i=1,c=1,r=1,C=1,q=2\x1b\\");
    }
    assert!(g.shown().any(|(k, _)| k == main[0]));
    // Back on the main screen, the alternate screen's images go at the next look.
    feed(&mut g, &mut t, b"\x1b[?1049l");
    g.sweep_at = 0;
    feed(&mut g, &mut t, b"\x1b[3;1H\x1b_Ga=p,i=1,c=1,r=1,q=2\x1b\\");
    assert_eq!(g.shown().count(), 2, "the main screen's two");
}

#[test]
fn a_removed_placement_goes_from_the_terminal_and_a_closed_pane_counts_as_a_change() {
    let s = shared(true);
    let mut g = PaneGraphics::new(s.clone());
    let mut t = term(20, 4);
    feed(&mut g, &mut t, &rgb_image("a=T,U=1,i=1,p=7,c=2,r=1,q=2", 2, 2));
    feed(&mut g, &mut t, b"\x1b_Ga=p,U=1,i=1,p=8,c=2,r=1,q=2\x1b\\");
    let live = |g: &PaneGraphics| -> HashMap<u32, Live> {
        g.shown().map(|(k, s)| (k, (s.generation, s.picture.clone(), s.placements.clone()))).collect()
    };
    let mut sent = HashMap::new();
    let mut out = Vec::new();
    assert!(!update_terminal(&mut sent, live(&g), usize::MAX, &mut out));
    let kid = *sent.keys().next().unwrap();
    // Placement 7 removed: the terminal loses the image's placements and gets 8 again.
    feed(&mut g, &mut t, b"\x1b_Ga=d,d=i,i=1,p=7\x1b\\");
    let mut out = Vec::new();
    update_terminal(&mut sent, live(&g), usize::MAX, &mut out);
    let text = String::from_utf8(out).unwrap();
    assert_eq!(
        text,
        format!("\x1b_Ga=d,d=i,i={kid},q=2\x1b\\\x1b_Ga=p,U=1,i={kid},p=8,c=2,r=1,q=2\x1b\\"),
        "no transmission again"
    );
    // The last one removed: the image goes from the terminal.
    feed(&mut g, &mut t, b"\x1b_Ga=d,d=i,i=1,p=8\x1b\\");
    assert_eq!(g.shown().count(), 0);
    let mut out = Vec::new();
    update_terminal(&mut sent, live(&g), usize::MAX, &mut out);
    assert_eq!(out, format!("\x1b_Ga=d,d=I,i={kid},q=2\x1b\\").into_bytes());
    // A pane that closes with images on show is a change clients see.
    feed(&mut g, &mut t, &rgb_image("a=T,i=2,c=1,r=1,q=2", 2, 2));
    let before = s.version();
    drop(g);
    assert_ne!(s.version(), before);
    assert_eq!(s.used(), 0);
}

// ---- terminals that draw sixel ---------------------------------------------------------------

#[test]
fn device_attributes_tell_the_terminals_that_draw_sixel() {
    assert!(probe().ends_with("\x1b[c"), "asked last: every terminal answers it");
    // foot, WezTerm, Windows Terminal, xterm as a VT340.
    for da in [
        "\x1b[?62;4;22;28c",
        "\x1b[?65;4;6;18;22c",
        "\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c",
        "\x1b[?63;1;2;3;4;6;9;15;16;22;29c",
    ] {
        assert_eq!(sixel_answer(da), Some(true), "{da:?}");
    }
    // kitty, Ghostty, xterm as it starts.
    for da in ["\x1b[?62;c", "\x1b[?62;22c", "\x1b[?64;1;2;6;9;15;16;17;18;21;22;28c"] {
        assert_eq!(sixel_answer(da), Some(false), "{da:?}");
    }
    // Other replies: the keyboard flags, the secondary attributes, a cursor report.
    for other in ["\x1b[?4u", "\x1b[>1;10;0c", "\x1b[?12;4R", "\x1b[?62;x;4c"] {
        assert_eq!(sixel_answer(other), None, "{other:?}");
    }
    let mut p = crate::input::InputParser::new();
    let mut events = p.feed(b"\x1b[6;20;10t\x1b[?62;4;22c");
    events.extend(p.flush());
    let raw: Vec<Vec<u8>> = events
        .into_iter()
        .filter_map(|e| match e {
            crate::input::InputEvent::Raw(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(raw, vec![b"\x1b[6;20;10t".to_vec(), b"\x1b[?62;4;22c".to_vec()]);
}

/// Image `kid`'s cells as `show_cells` leaves them, from the image's `col` and `row` at `area`.
fn image_cells(buf: &mut Buffer, area: Rect, kid: u32, col: u16, row: u16) {
    write_placeholders(buf, area, (area.x - col, area.y - row), kid, col + area.width, row + area.height);
}

fn part(kid: u32, area: Rect, col: u16, row: u16) -> paint::Part {
    paint::Part { kid, placement: 0, area, col, row }
}

fn by_place(mut parts: Vec<paint::Part>) -> Vec<paint::Part> {
    parts.sort_by_key(|p| (p.area.y, p.area.x));
    parts
}

#[test]
fn the_visible_parts_of_images_are_found_and_their_cells_blanked() {
    let (a, b) = (KRAAL_ID | 1, KRAAL_ID | 2);
    let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
    // Image a scrolled up by one row: its rows 1 to 4 show from row 1 of the screen.
    image_cells(&mut buf, Rect::new(2, 1, 6, 4), a, 0, 1);
    // A floating pane over three columns of its two middle rows.
    buf.set_string(5, 2, "+--", ratatui::style::Style::default());
    buf.set_string(5, 3, "|ab", ratatui::style::Style::default());
    // Image b on the last two rows, from its third column.
    image_cells(&mut buf, Rect::new(10, 8, 3, 2), b, 2, 0);
    let parts = paint::take_parts(&mut buf, Rect::new(0, 0, 20, 9));
    assert_eq!(
        by_place(parts),
        vec![
            part(a, Rect::new(2, 1, 6, 1), 0, 1),
            part(a, Rect::new(2, 2, 3, 2), 0, 2),
            part(a, Rect::new(2, 4, 6, 1), 0, 4),
            part(b, Rect::new(10, 8, 3, 1), 2, 0),
        ],
        "the last row is left out"
    );
    // Every image cell is blank now, those left out too; the rest is as it was.
    for y in 0..10 {
        for x in 0..20 {
            assert!(!buf[(x, y)].symbol().starts_with(PLACEHOLDER), "({x}, {y})");
        }
    }
    assert_eq!(buf[(10, 9)].symbol(), " ");
    assert_eq!(buf[(6, 3)].symbol(), "a");
}

#[test]
fn parts_are_drawn_when_they_change_and_erased_when_they_go() {
    let drawn = |area: Rect, row: u16| paint::Drawn {
        part: part(KRAAL_ID | 1, area, 0, row),
        picture: 1,
        cells: (6, 5),
        cell: (10, 20),
    };
    let mut painted = paint::Painted::default();
    let whole = drawn(Rect::new(2, 1, 6, 4), 0);
    assert!(painted.stale(std::slice::from_ref(&whole)).is_empty());
    assert_eq!(painted.update(vec![whole.clone()], |_| false), vec![whole.clone()], "new: drawn");
    // Nothing changed: nothing to do.
    assert!(painted.stale(std::slice::from_ref(&whole)).is_empty());
    assert!(painted.update(vec![whole.clone()], |_| false).is_empty());
    // Text written over its cells (a toast that came and went) takes it off: drawn again.
    assert_eq!(painted.update(vec![whole.clone()], |r| r.y == 1), vec![whole.clone()]);
    assert!(painted.update(vec![whole.clone()], |r| r.y == 0).is_empty());
    // Scrolled up by a row: where it was is erased, and it is drawn where it is.
    let up = drawn(Rect::new(2, 0, 6, 4), 1);
    assert_eq!(painted.stale(std::slice::from_ref(&up)), vec![whole.part.area]);
    assert_eq!(painted.update(vec![up.clone()], |_| false), vec![up.clone()]);
    // Partly covered by a floating pane: the parts left are drawn, the whole erased.
    let (top, left) = (drawn(Rect::new(2, 0, 6, 1), 1), drawn(Rect::new(2, 1, 3, 3), 2));
    let covered = vec![top.clone(), left.clone()];
    assert_eq!(painted.stale(&covered), vec![up.part.area]);
    assert_eq!(painted.update(covered.clone(), |_| false), covered);
    // Uncovered: whole again.
    assert_eq!(painted.stale(std::slice::from_ref(&up)), vec![top.part.area, left.part.area]);
    assert_eq!(painted.update(vec![up.clone()], |_| false), vec![up.clone()]);
    // Another picture, or cells of another size: drawn again in the same place.
    let other = paint::Drawn { picture: 2, ..up.clone() };
    assert_eq!(painted.stale(std::slice::from_ref(&other)), vec![up.part.area]);
    assert_eq!(painted.update(vec![other.clone()], |_| false), vec![other.clone()]);
    let bigger = paint::Drawn { cell: (9, 18), ..other.clone() };
    assert_eq!(painted.update(vec![bigger.clone()], |_| false), vec![bigger.clone()]);
    // A cleared terminal shows nothing: everything is drawn.
    painted.clear();
    assert!(painted.stale(&[]).is_empty());
    assert_eq!(painted.update(vec![bigger.clone()], |_| false), vec![bigger]);
    // The pixels of a part stop at the last whole band of six.
    assert_eq!(paint::pixels(&part(1, Rect::new(4, 2, 3, 2), 1, 2), (10, 20)), (10, 40, 30, 36));
    assert_eq!(paint::pixels(&part(1, Rect::new(4, 2, 3, 2), 1, 2), (9, 18)), (9, 36, 27, 36));
    assert_eq!(paint::pixels(&part(1, Rect::new(4, 2, 1, 1), 0, 0), (2, 5)), (0, 0, 2, 5));
    let mut out = Vec::new();
    paint::erase(Rect::new(4, 2, 3, 2), &mut out);
    assert_eq!(out, b"\x1b[3;5H\x1b[3X\x1b[4;5H\x1b[3X");
}

#[test]
fn an_image_that_moves_is_drawn_once_it_stays_put() {
    use std::time::{Duration, Instant};
    let still = Duration::from_millis(100);
    let ms = Duration::from_millis;
    let at =
        |area: Rect, row: u16| paint::Drawn { part: part(7, area, 0, row), picture: 1, cells: (4, 4), cell: (10, 20) };
    let mut motion = paint::Motion::default();
    let t0 = Instant::now();
    // Seen for the first time, or in the same place: drawn at once.
    assert!(motion.track(&[at(Rect::new(2, 10, 4, 4), 0)], t0, still).is_empty());
    assert!(motion.track(&[at(Rect::new(2, 10, 4, 4), 0)], t0 + ms(10), still).is_empty());
    // Output scrolls it up a row at a time: it waits until it has stayed put long enough.
    let t1 = t0 + ms(20);
    assert_eq!(motion.track(&[at(Rect::new(2, 9, 4, 4), 0)], t1, still), HashMap::from([(7, t1 + still)]));
    let t2 = t0 + ms(40);
    assert_eq!(motion.track(&[at(Rect::new(2, 8, 4, 4), 0)], t2, still), HashMap::from([(7, t2 + still)]));
    assert_eq!(motion.track(&[at(Rect::new(2, 8, 4, 4), 0)], t2 + ms(50), still), HashMap::from([(7, t2 + still)]));
    assert!(motion.track(&[at(Rect::new(2, 8, 4, 4), 0)], t2 + still, still).is_empty(), "drawn where it stopped");
    // Covered in part, or cut by the pane's top edge: it has not moved.
    let t3 = t2 + ms(200);
    assert!(motion.track(&[at(Rect::new(2, 9, 4, 3), 1)], t3, still).is_empty());
    assert!(motion.track(&[at(Rect::new(2, 8, 4, 1), 0), at(Rect::new(2, 10, 4, 2), 2)], t3, still).is_empty());
    // Gone, then back elsewhere: drawn at once.
    assert!(motion.track(&[], t3 + ms(10), still).is_empty());
    assert!(motion.track(&[at(Rect::new(2, 3, 4, 4), 0)], t3 + ms(20), still).is_empty());
}

#[test]
fn sixel_output_decodes_to_the_pixels_it_was_given() {
    use quantize::{CLEAR, Indexed};
    let palette = vec![[255, 0, 0], [0, 0, 255], [10, 200, 30]];
    let pixels: Vec<u8> = (0..5 * 8).map(|i| [0, 1, 2, CLEAR, 1][(i * 7 % 11) % 5]).collect();
    let image = Indexed { width: 5, height: 8, palette: palette.clone(), pixels: pixels.clone() };
    let check = |x: u32, y: u32, w: u32, h: u32| {
        let mut out = Vec::new();
        sixel::encode(&image, x, y, w, h, &mut out);
        let start = format!("\x1bP0;1;0q\"1;1;{w};{h}");
        assert!(out.starts_with(start.as_bytes()) && out.ends_with(b"\x1b\\"), "{:?}", String::from_utf8_lossy(&out));
        let (size, decoded) = sixel::decode(&out[b"\x1bP0;1;0q".len()..out.len() - 2]).unwrap();
        assert_eq!(size, (w, h));
        for py in 0..h {
            for px in 0..w {
                let want = pixels[((y + py) * 5 + x + px) as usize];
                let at = ((py * w + px) * 4) as usize;
                let got = &decoded[at..at + 4];
                if want == CLEAR {
                    assert_eq!(got[3], 0, "({px}, {py}) stays transparent");
                } else {
                    let c = palette[usize::from(want)];
                    assert!((0..3).all(|i| got[i].abs_diff(c[i]) <= 3) && got[3] == 255, "({px}, {py}): {got:?}");
                }
            }
        }
    };
    check(0, 0, 5, 8);
    check(1, 2, 3, 5);
    // Long runs are repeated, not written out; a part outside the image is nothing.
    let flat = Indexed { width: 40, height: 6, palette: vec![[0, 0, 0]], pixels: vec![0; 240] };
    let mut out = Vec::new();
    sixel::encode(&flat, 0, 0, 40, 6, &mut out);
    assert_eq!(out, b"\x1bP0;1;0q\"1;1;40;6#0;2;0;0;0#0!40~\x1b\\");
    let mut out = Vec::new();
    sixel::encode(&flat, 40, 0, 10, 6, &mut out);
    assert!(out.is_empty());
}

#[test]
fn pictures_are_fitted_to_their_cells_and_reduced_to_a_palette() {
    use quantize::{CLEAR, fit, quantize};
    // A few colours: the picture's own, transparent pixels apart.
    let (palette, indices) = quantize(&[255, 0, 0, 255, 0, 0, 255, 255, 255, 0, 0, 255, 9, 9, 9, 0]);
    assert_eq!(palette, vec![[255, 0, 0], [0, 0, 255]]);
    assert_eq!(indices, vec![0, 1, 0, CLEAR]);
    // Thousands: a palette learnt from them, close to each.
    let rgba: Vec<u8> = (0..64 * 64u32).flat_map(|i| [(i % 64 * 4) as u8, (i / 64 * 4) as u8, 128, 255]).collect();
    let (palette, indices) = quantize(&rgba);
    assert!(palette.len() <= 255);
    let error: u32 = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(&indices)
        .map(|(p, i)| (0..3).map(|c| u32::from(p[c].abs_diff(palette[usize::from(*i)][c]))).sum::<u32>())
        .sum();
    assert!(error / (64 * 64 * 3) <= 6, "mean error {}", error / (64 * 64 * 3));
    // Twice as wide as tall in a square box: scaled to its width, centred, the rest clear.
    let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([0, 255, 0, 255]));
    let fitted = fit(&image, 10, 10);
    assert_eq!((fitted.width, fitted.height, fitted.palette.clone()), (10, 10, vec![[0, 255, 0]]));
    let rows: Vec<bool> = fitted.pixels.chunks(10).map(|r| r.iter().all(|p| *p == 0)).collect();
    assert_eq!(rows, [false, false, true, true, true, true, true, false, false, false]);
    assert!(fitted.pixels[..20].iter().chain(&fitted.pixels[70..]).all(|p| *p == CLEAR));
}
