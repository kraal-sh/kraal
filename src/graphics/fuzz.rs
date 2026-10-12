//! Randomized tests of the images programs send (see `crate::fuzz`): kitty graphics commands and
//! sixel images, valid, damaged or hostile. Nothing panics, an image costs bounded time and
//! memory, the images a pane keeps stay within the quota and the limits, and the scanner takes
//! the same sequences out however reads cut them.

use std::time::{Duration, Instant};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use super::scan::Scanner;
use super::*;
use crate::fuzz::{self, Rng};

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

/// Image sequences and the text around them.
fn images(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..rng.range(1, 30) {
        match rng.below(6) {
            0..2 => fuzz::kitty(rng, &mut out),
            2 => fuzz::sixel(rng, &mut out),
            3 => {
                // A damaged one.
                let mut one = Vec::new();
                if rng.one_in(2) {
                    fuzz::kitty(rng, &mut one)
                } else {
                    fuzz::sixel(rng, &mut one)
                }
                rng.mutate(&mut one, &[b"\x1b\\", b"\x1b_G", b"\x1bPq", b";", b",", b"=", b"!", b"#", b"\""]);
                out.extend(one);
            }
            _ => fuzz::terminal_piece(rng, &mut out, false),
        }
    }
    out
}

#[test]
fn fuzz_graphics_commands() {
    fuzz::run("graphics_commands", 20, |rng| {
        let quota = *rng.pick(&[4 << 10, 256 << 10, 64 << 20]);
        let shared = Arc::new(Shared::new(true, quota));
        shared.set_available(!rng.one_in(6));
        shared.set_cell(rng.range(1, 40) as u16, rng.range(1, 60) as u16);
        // A pane is two columns wide at least: alacritty writes a wide character past one.
        let (cols, lines) = (rng.range(2, 200), rng.range(1, 60));
        let mut term = Term::new(Config::default(), &Size(cols, lines), VoidListener);
        let mut g = PaneGraphics::new(shared.clone());
        let mut parser: Processor = Processor::new();
        let output = images(rng);
        fuzz::trace(|| format!("output {:?}", output.escape_ascii().to_string()));
        let check = |reply: &[u8]| {
            assert!(reply.starts_with(b"\x1b_G") && reply.ends_with(b"\x1b\\"));
            assert!(!reply[3..reply.len() - 2].iter().any(|b| b.is_ascii_control()), "{reply:?}");
        };
        for chunk in rng.chunks(&output) {
            for piece in g.split(chunk) {
                let start = Instant::now();
                match piece {
                    Piece::Text(text) => parser.advance(&mut term, &text),
                    image => {
                        let what = format!("{image:?}").chars().take(200).collect::<String>();
                        if let Answer::Now(Some(reply)) = g.handle(image, &mut term) {
                            check(&reply);
                        }
                        let took = start.elapsed();
                        assert!(took < Duration::from_secs(10), "{what} took {took:?}");
                    }
                }
            }
            // What waits is prepared now and then, as the server's threads do.
            while rng.one_in(2)
                && let Some(work) = g.next_work()
            {
                let start = Instant::now();
                if let Some(Owed(Some(reply))) = g.prepared(work()) {
                    check(&reply);
                }
                assert!(start.elapsed() < Duration::from_secs(10));
            }
            // This pane is the only one: it keeps within the quota.
            assert!(shared.used() <= quota, "{} bytes held, for a quota of {quota}", shared.used());
            assert!(g.shown.len() <= MAX_SHOWN);
            assert!(g.shown.values().all(|s| s.placements.len() <= MAX_PLACEMENTS));
            assert!(g.pending.as_ref().is_none_or(|p| p.payload.len() <= MAX_UPLOAD));
        }
        let area = Rect::new(0, 0, cols as u16, lines as u16);
        let mut buf = Buffer::empty(area);
        crate::pane::render_term(&term, area, &mut buf, None);
        g.show_cells(area, &mut buf, true);
        let _ = g.shown().count();
        drop(g);
        assert_eq!(shared.used(), 0, "the pictures of a closed pane are freed");
    });
}

#[test]
fn fuzz_graphics_sixel() {
    fuzz::run("graphics_sixel", 300, |rng| {
        let mut data = Vec::new();
        fuzz::sixel(rng, &mut data);
        // Without its introducer and terminator.
        let start = data.iter().position(|b| *b == b'q').map_or(0, |i| i + 1);
        let mut data = data[start..].to_vec();
        if rng.one_in(2) {
            rng.mutate(&mut data, &[b"!", b"#", b"\"", b"-", b"$", b";", b"9999999999"]);
        }
        let begin = Instant::now();
        if let Some(((width, height), pixels)) = sixel::decode(&data) {
            assert!(within_limits(width, height), "{width}x{height}");
            assert_eq!(pixels.len(), width as usize * height as usize * 4);
            if pixels.len() <= 1 << 20 {
                // What a terminal that draws sixel gets back.
                let rgba = image::RgbaImage::from_raw(width, height, pixels).unwrap();
                let indexed = quantize::fit(&rgba, width.min(64), height.min(64));
                let mut out = Vec::new();
                sixel::encode(&indexed, 0, 0, indexed.width, indexed.height, &mut out);
                // The data, without the introducer and the terminator (the scanner takes them out).
                let data = &out[out.iter().position(|b| *b == b'q').unwrap() + 1..out.len() - 2];
                if let Some(((again_width, again_height), _)) = sixel::decode(data) {
                    assert!(
                        again_width <= indexed.width && again_height <= indexed.height,
                        "{again_width}x{again_height} from {}x{}: {:?}",
                        indexed.width,
                        indexed.height,
                        String::from_utf8_lossy(&out)
                    );
                }
            }
        }
        let took = begin.elapsed();
        assert!(took < Duration::from_secs(10), "{} bytes took {took:?}", data.len());
    });
}

#[test]
fn fuzz_graphics_scanner() {
    fuzz::run("graphics_scanner", 200, |rng| {
        let output = images(rng);
        let whole: Vec<Piece> = Scanner::default().split(&output);
        let mut s = Scanner::default();
        let mut cut: Vec<Piece> = Vec::new();
        for chunk in rng.chunks(&output) {
            cut.extend(s.split(chunk));
        }
        let text = |pieces: &[Piece]| -> Vec<u8> {
            pieces.iter().flat_map(|p| if let Piece::Text(t) = p { t.to_vec() } else { Vec::new() }).collect()
        };
        let imgs = |pieces: &[Piece]| -> Vec<String> {
            pieces.iter().filter(|p| !matches!(p, Piece::Text(_))).map(|p| format!("{p:?}")).collect()
        };
        // What waits at the end (a sequence not ended) waits in both.
        assert_eq!(imgs(&whole), imgs(&cut), "the same images");
        assert_eq!(text(&whole), text(&cut), "the same text");
    });
}

#[test]
fn fuzz_graphics_command_parse() {
    fuzz::run("graphics_command_parse", 500, |rng| {
        let mut body = Vec::new();
        fuzz::kitty(rng, &mut body);
        let mut body = body[3..].to_vec();
        rng.mutate(&mut body, &[b",", b"=", b";", b"a=", b"i=", b"9999999999999999999999"]);
        if let Ok(cmd) = Command::parse(&body) {
            let _ = cmd.reply(cmd.id, "EINVAL:\x1b]0;x\x07".into());
        }
    });
}
