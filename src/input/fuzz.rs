//! Randomized tests of what the client's terminal sends (see `crate::fuzz`): the parser never
//! panics, reads the same events however the reads cut the input, keeps what waits bounded, and
//! what it reads encodes for a pane without panicking.

use super::*;
use crate::fuzz::{self, Rng};

/// Input a terminal sends: keys in every encoding, mouse reports, pastes, focus, replies to
/// queries, and bytes of any kind.
fn terminal_input(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..rng.size(60) {
        match rng.below(16) {
            0..4 => out.extend(rng.text(20)),
            4 => out.push(rng.below(0x20) as u8),
            5 => out.extend_from_slice(rng.one_of(&[
                &b"\x1b"[..],
                b"\x1b\x1b",
                b"\x1b[",
                b"\x1bO",
                b"\x1b]",
                b"\x1bP",
                b"\x1b_",
                b"\x1b^",
                b"\x1bX",
            ])),
            6..8 => {
                // A CSI key: legacy, kitty (CSI u) or modifyOtherKeys, fields of any size.
                out.extend_from_slice(b"\x1b[");
                if rng.one_in(6) {
                    out.push(*rng.pick(b"<=>?"));
                }
                for i in 0..rng.size(4) {
                    if i > 0 {
                        out.push(b';');
                    }
                    for j in 0..rng.range(1, 3) {
                        if j > 0 {
                            out.push(b':');
                        }
                        if !rng.one_in(5) {
                            out.extend_from_slice(rng.number().to_string().as_bytes());
                        }
                    }
                }
                out.push(*rng.pick(b"ABCDEFHPQRSZuIOM~m$^@"));
            }
            8 => {
                let (b, x, y) = (rng.number() % 300, rng.number() % 70_000, rng.number() % 70_000);
                out.extend_from_slice(format!("\x1b[<{b};{x};{y}{}", rng.pick(&["M", "m"])).as_bytes());
            }
            9 => {
                out.extend_from_slice(b"\x1b[M");
                for _ in 0..3 {
                    out.push(rng.range(0, 255) as u8);
                }
            }
            10 => out.extend_from_slice(rng.one_of(&[
                &b"\x1b[I"[..],
                b"\x1b[O",
                b"\x1bOA",
                b"\x1bO5P",
                b"\x1bOp",
                b"\x1bO",
            ])),
            11..13 => {
                // A paste: its end marker may come late, cut, or never.
                out.extend_from_slice(b"\x1b[200~");
                for _ in 0..rng.size(20) {
                    match rng.below(5) {
                        0 => out.extend_from_slice(b"\x1b[201"),
                        1 => out.extend_from_slice(b"\x1b[200~"),
                        2 => out.extend(vec![b'p'; rng.size(5000)]),
                        _ => out.extend(rng.text(40)),
                    }
                }
                if !rng.one_in(4) {
                    out.extend_from_slice(b"\x1b[201~");
                }
            }
            13 => {
                // A reply to a query: OSC, DCS, APC strings, terminated or not, long or not.
                out.extend_from_slice(rng.one_of(&[
                    &b"\x1b]11;rgb:0/0/0"[..],
                    b"\x1bP>|kitty",
                    b"\x1b_Gi=1;OK",
                    b"\x1b]52;c;",
                ]));
                let n = rng.size(70_000);
                out.extend(std::iter::repeat_n(b'A', n));
                out.extend_from_slice(rng.one_of(&[&b"\x07"[..], b"\x1b\\", b"", b"\x1bx"]));
            }
            _ => {
                let n = rng.size(16);
                out.extend(rng.bytes(n));
            }
        }
    }
    out
}

/// The events, with each paste made whole: how a paste is cut into pieces depends on the reads.
fn whole_pastes(events: Vec<InputEvent>) -> Vec<InputEvent> {
    let mut out = Vec::new();
    let mut paste: Option<Vec<u8>> = None;
    for ev in events {
        match ev {
            InputEvent::PastePiece(p) => paste.get_or_insert_with(Vec::new).extend(p),
            InputEvent::Paste(p) => {
                let mut all = paste.take().unwrap_or_default();
                all.extend(p);
                out.push(InputEvent::Paste(all));
            }
            ev => out.push(ev),
        }
    }
    if let Some(p) = paste {
        out.push(InputEvent::PastePiece(p));
    }
    out
}

const MODES: [PaneInputModes; 4] = [
    PaneInputModes {
        app_cursor: false,
        app_keypad: false,
        bracketed_paste: false,
        mouse_click: false,
        mouse_drag: false,
        mouse_motion: false,
        sgr_mouse: false,
        utf8_mouse: false,
        focus_events: false,
        alternate_scroll: false,
        alt_screen: false,
        kitty_flags: 0,
    },
    PaneInputModes {
        app_cursor: true,
        app_keypad: true,
        bracketed_paste: true,
        mouse_click: true,
        mouse_drag: true,
        mouse_motion: true,
        sgr_mouse: true,
        utf8_mouse: false,
        focus_events: true,
        alternate_scroll: true,
        alt_screen: true,
        kitty_flags: 1,
    },
    PaneInputModes {
        app_cursor: false,
        app_keypad: false,
        bracketed_paste: true,
        mouse_click: true,
        mouse_drag: false,
        mouse_motion: true,
        sgr_mouse: false,
        utf8_mouse: true,
        focus_events: false,
        alternate_scroll: true,
        alt_screen: true,
        kitty_flags: 31,
    },
    PaneInputModes {
        app_cursor: false,
        app_keypad: true,
        bracketed_paste: false,
        mouse_click: true,
        mouse_drag: true,
        mouse_motion: false,
        sgr_mouse: false,
        utf8_mouse: false,
        focus_events: true,
        alternate_scroll: false,
        alt_screen: false,
        kitty_flags: 8,
    },
];

#[test]
fn fuzz_input_decode() {
    fuzz::run("input_decode", 200, |rng| {
        let mut input = terminal_input(rng);
        if rng.one_in(3) {
            rng.mutate(&mut input, &[b"\x1b[200~", b"\x1b[201~", b"\x1b[", b";", b":", b"u", b"\x1b"]);
        }
        fuzz::trace(|| format!("input {:?}", input.escape_ascii().to_string()));
        let mut whole = InputParser::new();
        let mut once = whole.feed(&input);
        once.extend(whole.flush());
        let mut parser = InputParser::new();
        let mut events = Vec::new();
        for chunk in rng.chunks(&input) {
            events.extend(parser.feed(chunk));
            // What waits for the rest of a sequence stays bounded (a string, a paste piece).
            let (waiting, pasted) = parser.waiting();
            assert!(waiting <= 70_000 + chunk.len(), "{waiting} bytes wait");
            assert!(pasted < (1 << 20) + chunk.len(), "{pasted} bytes of a paste wait");
        }
        events.extend(parser.flush());
        assert_eq!(whole_pastes(events.clone()), whole_pastes(once), "the reads changed the events");
        for ev in &events {
            for modes in MODES {
                match ev {
                    InputEvent::Key(key) => {
                        let _ = encode_key(key, modes);
                        let _ = key.by_place();
                        let _ = key.has_place();
                    }
                    InputEvent::Mouse(m) => {
                        let _ = encode_mouse(m, m.col, m.row, modes);
                        let _ = encode_alternate_scroll(m, modes);
                    }
                    InputEvent::Paste(p) | InputEvent::PastePiece(p) => {
                        let _ = encode_paste(p, modes);
                        let _ = encode_paste_after(p, modes, rng.one_in(2));
                    }
                    InputEvent::KittyKey { raw, release } => {
                        let _ = kitty_key_flag(raw, *release);
                    }
                    _ => {}
                }
            }
        }
    });
}

#[test]
fn fuzz_input_key_specs() {
    fuzz::run("input_key_specs", 300, |rng| {
        let mut spec = rng
            .pick(&["ctrl+a", "alt+shift+F12", "super+enter", "C-x", "ctrl+alt+[", "space", "f20", "shift+tab", "M-é"])
            .as_bytes()
            .to_vec();
        rng.mutate(&mut spec, &[b"+", b"-", b"ctrl", b"alt", b"shift", b"f", b"9"]);
        if let Ok(spec) = KeySpec::parse(&String::from_utf8_lossy(&spec)) {
            let key = Key { code: KeyCode::Char('a'), mods: Mods::CTRL, raw: Vec::new() };
            let _ = spec.matches(&key);
        }
    });
}
