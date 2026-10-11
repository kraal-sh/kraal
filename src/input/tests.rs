//! Tests of terminal input: parsing every key, mouse and paste form, encoding for panes, and
//! key specifications.

use super::KeyCode::*;
use super::decode::*;
use super::encode::*;
use super::keyspec::*;
use super::*;

const NONE: Mods = Mods::NONE;
const SHIFT: Mods = Mods::SHIFT;
const ALT: Mods = Mods::ALT;
const CTRL: Mods = Mods::CTRL;

/// Modifiers from letters: `s` shift, `a` alt, `c` ctrl.
fn mods(s: &str) -> Mods {
    Mods { shift: s.contains('s'), alt: s.contains('a'), ctrl: s.contains('c') }
}

fn key_ev(code: KeyCode, mods: Mods, raw: &[u8]) -> InputEvent {
    InputEvent::Key(Key { code, mods, raw: raw.to_vec() })
}

fn raw_ev(raw: &[u8]) -> InputEvent {
    InputEvent::Raw(raw.to_vec())
}

fn mouse_ev(kind: MouseKind, col: u16, row: u16, mods: Mods) -> InputEvent {
    InputEvent::Mouse(MouseEvent { kind, col, row, mods })
}

/// Events of `bytes`, as the server gets them: what is complete, then what the flush timeout
/// turns the rest into.
fn parse_all(bytes: &[u8]) -> Vec<InputEvent> {
    let mut parser = InputParser::new();
    let mut events = parser.feed(bytes);
    events.extend(parser.flush());
    events
}

fn parse_key(bytes: &[u8]) -> Key {
    match parse_all(bytes).as_slice() {
        [InputEvent::Key(k)] => {
            assert_eq!(k.raw, bytes, "raw bytes of {bytes:?}");
            k.clone()
        }
        other => panic!("{bytes:?}: expected one key, got {other:?}"),
    }
}

fn single_key(bytes: &[u8]) -> (KeyCode, Mods) {
    let k = parse_key(bytes);
    (k.code, k.mods)
}

fn single_mouse(bytes: &[u8]) -> MouseEvent {
    match parse_all(bytes).as_slice() {
        [InputEvent::Mouse(m)] => *m,
        other => panic!("{bytes:?}: expected one mouse event, got {other:?}"),
    }
}

fn feed_chunks(chunks: &[&[u8]]) -> Vec<InputEvent> {
    let mut parser = InputParser::new();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.feed(chunk));
    }
    assert!(!parser.has_pending(), "pending after {chunks:?}");
    events
}

// ---- Parsing: plain bytes ----

#[test]
fn printable_ascii() {
    assert_eq!(single_key(b"a"), (Char('a'), NONE));
    assert_eq!(single_key(b"z"), (Char('z'), NONE));
    assert_eq!(single_key(b"A"), (Char('A'), SHIFT));
    assert_eq!(single_key(b"%"), (Char('%'), NONE));
    assert_eq!(single_key(b" "), (Char(' '), NONE));
    assert_eq!(single_key(b"~"), (Char('~'), NONE));
    assert_eq!(parse_all(b"hi"), vec![key_ev(Char('h'), NONE, b"h"), key_ev(Char('i'), NONE, b"i")]);
}

#[test]
fn utf8_multibyte() {
    for s in ["é", "€", "日", "😀"] {
        assert_eq!(single_key(s.as_bytes()), (Char(s.chars().next().unwrap()), NONE));
    }
}

#[test]
fn utf8_split_across_chunks() {
    let data = "é😀日".as_bytes();
    let whole = parse_all(data);
    assert_eq!(whole.len(), 3);
    for i in 0..=data.len() {
        assert_eq!(feed_chunks(&[&data[..i], &data[i..]]), whole, "split at {i}");
    }
    let mut parser = InputParser::new();
    let one_by_one: Vec<_> = data.iter().flat_map(|b| parser.feed(&[*b])).collect();
    assert_eq!(one_by_one, whole);

    let mut parser = InputParser::new();
    assert!(parser.feed(&data[2..4]).is_empty());
    assert!(parser.has_pending());
}

#[test]
fn control_bytes() {
    assert_eq!(single_key(b"\x00"), (Char(' '), CTRL));
    assert_eq!(single_key(b"\x08"), (Char('h'), CTRL));
    assert_eq!(single_key(b"\x09"), (Tab, NONE));
    assert_eq!(single_key(b"\x0a"), (Char('j'), CTRL));
    assert_eq!(single_key(b"\x0d"), (Enter, NONE));
    assert_eq!(single_key(b"\x1c"), (Char('\\'), CTRL));
    assert_eq!(single_key(b"\x1d"), (Char(']'), CTRL));
    assert_eq!(single_key(b"\x1e"), (Char('^'), CTRL));
    assert_eq!(single_key(b"\x1f"), (Char('_'), CTRL));
    assert_eq!(single_key(b"\x7f"), (Backspace, NONE));
    for (byte, c) in (1u8..=26).zip('a'..='z') {
        if byte != 9 && byte != 13 {
            assert_eq!(single_key(&[byte]), (Char(c), CTRL), "byte {byte}");
        }
    }
}

#[test]
fn alt_prefixed_keys() {
    assert_eq!(single_key(b"\x1bh"), (Char('h'), ALT));
    assert_eq!(single_key(b"\x1bH"), (Char('H'), mods("sa")));
    assert_eq!(single_key(b"\x1b%"), (Char('%'), ALT));
    assert_eq!(single_key(b"\x1b\x18"), (Char('x'), mods("ac")));
    assert_eq!(single_key(b"\x1b\x00"), (Char(' '), mods("ac")));
    assert_eq!(single_key(b"\x1b\x7f"), (Backspace, ALT));
    assert_eq!(single_key(b"\x1b\r"), (Enter, ALT));
    assert_eq!(single_key(b"\x1b\t"), (Tab, ALT));
    assert_eq!(single_key("\x1bé".as_bytes()), (Char('é'), ALT));
    assert_eq!(single_key(b"\x1b\x1b[A"), (Up, ALT));
    assert_eq!(single_key(b"\x1b\x1bOP"), (F(1), ALT));
    assert_eq!(single_key(b"\x1b\x1b[1;5A"), (Up, mods("ac")));
    assert_eq!(single_key(b"\x1b\x1b[3~"), (Delete, ALT));
}

#[test]
fn lone_esc_at_chunk_end() {
    let mut parser = InputParser::new();
    // Escape waits for the flush timeout…
    assert!(parser.feed(b"\x1b").is_empty());
    assert!(parser.has_pending());
    assert_eq!(parser.flush(), vec![key_ev(Esc, NONE, b"\x1b")]);
    assert_eq!(parser.feed(b"a\x1b"), vec![key_ev(Char('a'), NONE, b"a")]);
    assert_eq!(parser.flush(), vec![key_ev(Esc, NONE, b"\x1b")]);
    // …so a sequence split right after its ESC is reassembled: a paste stays a paste.
    assert!(parser.feed(b"\x1b").is_empty());
    assert_eq!(parser.feed(b"[A"), vec![key_ev(Up, NONE, b"\x1b[A")]);
    assert!(parser.feed(b"\x1b").is_empty());
    assert_eq!(parser.feed(b"[200~echo hi\n\x1b[201~"), vec![InputEvent::Paste(b"echo hi\n".to_vec())]);
}

#[test]
fn double_esc() {
    let mut parser = InputParser::new();
    assert!(parser.feed(b"\x1b\x1b").is_empty());
    assert!(parser.has_pending());
    assert_eq!(parser.flush(), vec![key_ev(Esc, ALT, b"\x1b\x1b")]);
    assert!(!parser.has_pending());

    assert_eq!(parse_all(b"\x1b\x1bx"), vec![key_ev(Esc, ALT, b"\x1b\x1b"), key_ev(Char('x'), NONE, b"x")]);
    assert_eq!(
        parse_all(b"\x1b\x1b[<0;1;1M"),
        vec![key_ev(Esc, NONE, b"\x1b"), mouse_ev(MouseKind::Press(MouseButton::Left), 0, 0, NONE)]
    );
    assert_eq!(
        parse_all(b"\x1b\x1b[200~hi\x1b[201~"),
        vec![key_ev(Esc, NONE, b"\x1b"), InputEvent::Paste(b"hi".to_vec())]
    );
    assert_eq!(
        parse_all(b"\x1b\x1bOa"),
        vec![key_ev(Esc, NONE, b"\x1b"), key_ev(Char('O'), mods("sa"), b"\x1bO"), key_ev(Char('a'), NONE, b"a"),]
    );
}

// ---- Parsing: CSI / SS3 keys ----

#[test]
fn cursor_keys() {
    for (seq, code) in [
        (&b"\x1b[A"[..], Up),
        (b"\x1b[B", Down),
        (b"\x1b[C", Right),
        (b"\x1b[D", Left),
        (b"\x1b[H", Home),
        (b"\x1b[F", End),
        (b"\x1bOA", Up),
        (b"\x1bOB", Down),
        (b"\x1bOC", Right),
        (b"\x1bOD", Left),
        (b"\x1bOH", Home),
        (b"\x1bOF", End),
        (b"\x1b[1;1A", Up),
    ] {
        assert_eq!(single_key(seq), (code, NONE), "{seq:?}");
    }
    assert_eq!(single_key(b"\x1b[1;2A"), (Up, SHIFT));
    assert_eq!(single_key(b"\x1b[1;3B"), (Down, ALT));
    assert_eq!(single_key(b"\x1b[1;4C"), (Right, mods("sa")));
    assert_eq!(single_key(b"\x1b[1;5D"), (Left, CTRL));
    assert_eq!(single_key(b"\x1b[1;6H"), (Home, mods("sc")));
    assert_eq!(single_key(b"\x1b[1;7F"), (End, mods("ac")));
    assert_eq!(single_key(b"\x1b[1;8A"), (Up, mods("sac")));
    // Meta is folded into alt.
    assert_eq!(single_key(b"\x1b[1;9A"), (Up, ALT));
    // Old xterm SS3 with modifier digit.
    assert_eq!(single_key(b"\x1bO5A"), (Up, CTRL));
}

#[test]
fn tilde_and_function_keys() {
    for (seq, code) in [
        (&b"\x1b[1~"[..], Home),
        (b"\x1b[2~", Insert),
        (b"\x1b[3~", Delete),
        (b"\x1b[4~", End),
        (b"\x1b[5~", PageUp),
        (b"\x1b[6~", PageDown),
        (b"\x1b[7~", Home),
        (b"\x1b[8~", End),
        (b"\x1bOP", F(1)),
        (b"\x1bOQ", F(2)),
        (b"\x1bOR", F(3)),
        (b"\x1bOS", F(4)),
        (b"\x1b[11~", F(1)),
        (b"\x1b[14~", F(4)),
        (b"\x1b[15~", F(5)),
        (b"\x1b[17~", F(6)),
        (b"\x1b[18~", F(7)),
        (b"\x1b[19~", F(8)),
        (b"\x1b[20~", F(9)),
        (b"\x1b[21~", F(10)),
        (b"\x1b[23~", F(11)),
        (b"\x1b[24~", F(12)),
        (b"\x1b[25~", F(13)),
        (b"\x1b[34~", F(20)),
    ] {
        assert_eq!(single_key(seq), (code, NONE), "{seq:?}");
    }
    assert_eq!(single_key(b"\x1b[2;5~"), (Insert, CTRL));
    assert_eq!(single_key(b"\x1b[3;2~"), (Delete, SHIFT));
    assert_eq!(single_key(b"\x1b[5;3~"), (PageUp, ALT));
    assert_eq!(single_key(b"\x1b[24;2~"), (F(12), SHIFT));
    assert_eq!(single_key(b"\x1b[15;8~"), (F(5), mods("sac")));
    assert_eq!(single_key(b"\x1b[1;2P"), (F(1), SHIFT));
    assert_eq!(single_key(b"\x1b[1;5S"), (F(4), CTRL));
    assert_eq!(single_key(b"\x1b[P"), (F(1), NONE));
}

#[test]
fn backtab() {
    assert_eq!(single_key(b"\x1b[Z"), (BackTab, SHIFT));
    assert_eq!(single_key(b"\x1b[1;6Z"), (BackTab, mods("sc")));
    assert_eq!(single_key(b"\x1b[1;3Z"), (BackTab, mods("sa")));
}

#[test]
fn keypad_ss3() {
    assert_eq!(single_key(b"\x1bOM"), (Enter, NONE));
    assert_eq!(single_key(b"\x1bOp"), (Char('0'), NONE));
    assert_eq!(single_key(b"\x1bOy"), (Char('9'), NONE));
    assert_eq!(single_key(b"\x1bOk"), (Char('+'), NONE));
    assert_eq!(single_key(b"\x1bOX"), (Char('='), NONE));
}

#[test]
fn csi_u_and_modify_other_keys() {
    assert_eq!(single_key(b"\x1b[97;5u"), (Char('a'), CTRL));
    assert_eq!(single_key(b"\x1b[97;2u"), (Char('A'), SHIFT));
    assert_eq!(single_key(b"\x1b[97;6u"), (Char('a'), mods("sc")));
    assert_eq!(single_key(b"\x1b[65;5u"), (Char('a'), mods("sc")));
    assert_eq!(single_key(b"\x1b[97u"), (Char('a'), NONE));
    assert_eq!(single_key(b"\x1b[49:33;2u"), (Char('!'), SHIFT));
    assert_eq!(single_key(b"\x1b[13;3u"), (Enter, ALT));
    assert_eq!(single_key(b"\x1b[9;2u"), (BackTab, SHIFT));
    assert_eq!(single_key(b"\x1b[9u"), (Tab, NONE));
    assert_eq!(single_key(b"\x1b[27u"), (Esc, NONE));
    assert_eq!(single_key(b"\x1b[127;5u"), (Backspace, CTRL));
    assert_eq!(single_key(b"\x1b[32;5u"), (Char(' '), CTRL));
    assert_eq!(single_key(b"\x1b[233u"), (Char('é'), NONE));
    assert_eq!(single_key(b"\x1b[97;5:1u"), (Char('a'), CTRL));
    assert_eq!(single_key(b"\x1b[27;5;97~"), (Char('a'), CTRL));
    assert_eq!(single_key(b"\x1b[27;2;33~"), (Char('!'), SHIFT));
    // Keypad keys type what they show; release events and other kitty functional keys (here
    // Ctrl+Print Screen with Num Lock on) are kept apart for the programs that asked for them.
    assert_eq!(single_key(b"\x1b[57399u"), (Char('0'), NONE));
    assert_eq!(single_key(b"\x1b[57400;129u"), (Char('1'), NONE));
    // Caps Lock and Num Lock ride along on legacy keys too.
    assert_eq!(single_key(b"\x1b[1;129A"), (Up, NONE));
    assert_eq!(single_key(b"\x1b[1;65B"), (Down, NONE));
    assert_eq!(single_key(b"\x1b[1;133D"), (Left, CTRL));
    assert_eq!(single_key(b"\x1b[5;129~"), (PageUp, NONE));
    assert_eq!(parse_all(b"\x1b[1;145A"), vec![raw_ev(b"\x1b[1;145A")]);
    // Not for R: `CSI 1;129 R` is a cursor position report, not F3.
    assert_eq!(parse_all(b"\x1b[1;129R"), vec![raw_ev(b"\x1b[1;129R")]);
    assert_eq!(single_key(b"\x1b[57413u"), (Char('+'), NONE));
    assert_eq!(single_key(b"\x1b[57414u"), (Enter, NONE));
    assert_eq!(single_key(b"\x1b[57419;5u"), (Up, CTRL));
    let kitty = |raw: &[u8], release| vec![InputEvent::KittyKey { raw: raw.to_vec(), release }];
    assert_eq!(parse_all(b"\x1b[57361;133u"), kitty(b"\x1b[57361;133u", false));
    for release in [&b"\x1b[97;5:3u"[..], b"\x1b[1;129:3A", b"\x1b[6;129:3~"] {
        assert_eq!(parse_all(release), kitty(release, true));
    }
    assert_eq!(parse_all(b"\x1b[>1u"), vec![raw_ev(b"\x1b[>1u")]);
    assert_eq!(parse_all(b"\x1b[?1u"), vec![raw_ev(b"\x1b[?1u")]);
}

#[test]
fn focus_events() {
    assert_eq!(parse_all(b"\x1b[I"), vec![InputEvent::FocusGained]);
    assert_eq!(parse_all(b"\x1b[O"), vec![InputEvent::FocusLost]);
}

// ---- Parsing: mouse ----

#[test]
fn sgr_mouse() {
    use MouseButton as B;
    use MouseKind as K;
    let cases: &[(&[u8], MouseKind, u16, u16, Mods)] = &[
        (b"\x1b[<0;1;1M", K::Press(B::Left), 0, 0, NONE),
        (b"\x1b[<1;10;20M", K::Press(B::Middle), 9, 19, NONE),
        (b"\x1b[<2;3;4M", K::Press(B::Right), 2, 3, NONE),
        (b"\x1b[<0;3;4m", K::Release(B::Left), 2, 3, NONE),
        (b"\x1b[<1;3;4m", K::Release(B::Middle), 2, 3, NONE),
        (b"\x1b[<2;3;4m", K::Release(B::Right), 2, 3, NONE),
        (b"\x1b[<32;5;6M", K::Drag(B::Left), 4, 5, NONE),
        (b"\x1b[<33;5;6M", K::Drag(B::Middle), 4, 5, NONE),
        (b"\x1b[<34;5;6M", K::Drag(B::Right), 4, 5, NONE),
        (b"\x1b[<35;5;6M", K::Move, 4, 5, NONE),
        (b"\x1b[<64;1;1M", K::ScrollUp, 0, 0, NONE),
        (b"\x1b[<65;1;1M", K::ScrollDown, 0, 0, NONE),
        (b"\x1b[<66;1;1M", K::ScrollLeft, 0, 0, NONE),
        (b"\x1b[<67;1;1M", K::ScrollRight, 0, 0, NONE),
        (b"\x1b[<4;1;1M", K::Press(B::Left), 0, 0, SHIFT),
        (b"\x1b[<8;1;1M", K::Press(B::Left), 0, 0, ALT),
        (b"\x1b[<16;1;1M", K::Press(B::Left), 0, 0, CTRL),
        (b"\x1b[<84;7;8M", K::ScrollUp, 6, 7, mods("sc")),
        (b"\x1b[<60;7;8M", K::Drag(B::Left), 6, 7, mods("sac")),
        (b"\x1b[<0;300;1000M", K::Press(B::Left), 299, 999, NONE),
        (b"\x1b[<0;99999;1M", K::Press(B::Left), u16::MAX, 0, NONE),
    ];
    for &(seq, kind, col, row, m) in cases {
        assert_eq!(single_mouse(seq), MouseEvent { kind, col, row, mods: m }, "{seq:?}");
    }
    for bad in [&b"\x1b[<128;1;1M"[..], b"\x1b[<0;1M", b"\x1b[<3;1;1M"] {
        assert_eq!(parse_all(bad), vec![raw_ev(bad)]);
    }
}

#[test]
fn x10_mouse() {
    use MouseButton as B;
    use MouseKind as K;
    let mut parser = InputParser::new();
    let events = parser.feed(b"\x1b[M\x21\x25\x23\x1b[M\x23\x26\x23\x1b[M\x60\x21\x21\x1b[M\x40\x22\x21");
    assert_eq!(
        events,
        vec![
            mouse_ev(K::Press(B::Middle), 4, 2, NONE),
            mouse_ev(K::Release(B::Middle), 5, 2, NONE),
            mouse_ev(K::ScrollUp, 0, 0, NONE),
            mouse_ev(K::Drag(B::Left), 1, 0, NONE),
        ]
    );
    // Split inside the report.
    let mut parser = InputParser::new();
    assert!(parser.feed(b"\x1b[M\x20").is_empty());
    assert_eq!(parser.feed(b"\x21\x21"), vec![mouse_ev(K::Press(B::Left), 0, 0, NONE)]);
}

// ---- Parsing: paste ----

#[test]
fn paste_single_chunk() {
    assert_eq!(parse_all(b"\x1b[200~hello\nworld\x1b[201~"), vec![InputEvent::Paste(b"hello\nworld".to_vec())]);
    assert_eq!(
        parse_all(b"a\x1b[200~x\x1b[A\x1by\x01\x1b[201~b"),
        vec![
            key_ev(Char('a'), NONE, b"a"),
            InputEvent::Paste(b"x\x1b[A\x1by\x01".to_vec()),
            key_ev(Char('b'), NONE, b"b"),
        ]
    );
    // Empty pastes carry nothing and are dropped.
    assert_eq!(parse_all(b"\x1b[200~\x1b[201~z"), vec![key_ev(Char('z'), NONE, b"z")]);
}

#[test]
fn paste_spanning_chunks() {
    let mut parser = InputParser::new();
    assert!(parser.feed(b"\x1b[200~first ").is_empty());
    assert!(parser.has_pending());
    assert!(parser.feed(b"second\n").is_empty());
    assert!(parser.feed(b"third\x1b[20").is_empty());
    assert_eq!(
        parser.feed(b"1~ok"),
        vec![
            InputEvent::Paste(b"first second\nthird".to_vec()),
            key_ev(Char('o'), NONE, b"o"),
            key_ev(Char('k'), NONE, b"k"),
        ]
    );
    assert!(!parser.has_pending());

    // Something that looks like the end marker but is not.
    assert_eq!(feed_chunks(&[b"\x1b[200~a\x1b[20", b"2~b\x1b[201~"]), vec![InputEvent::Paste(b"a\x1b[202~b".to_vec())]);

    // Every split position of a paste wrapped in keys.
    let data = b"x\x1b[200~p\x1b[Aq\r\nr\x1b[201~y";
    let whole = parse_all(data);
    assert_eq!(whole.len(), 3);
    for i in 1..=data.len() {
        if data[i - 1] == ESC && i - 1 == 1 {
            continue; // lone ESC rule
        }
        assert_eq!(feed_chunks(&[&data[..i], &data[i..]]), whole, "split at {i}");
    }
}

#[test]
fn flush_during_paste() {
    let mut parser = InputParser::new();
    assert!(parser.feed(b"\x1b[200~abc\x1b[2").is_empty());
    assert!(parser.has_pending());
    assert_eq!(parser.flush(), vec![InputEvent::PastePiece(b"abc".to_vec())]);
    assert!(!parser.has_pending());
    assert!(parser.flush().is_empty());
    // Still inside the paste: the marker prefix was kept, and the end is told even with nothing
    // left to paste.
    assert_eq!(parser.feed(b"01~"), vec![InputEvent::Paste(Vec::new())]);
    assert_eq!(parser.feed(b"q"), vec![key_ev(Char('q'), NONE, b"q")]);

    let mut parser = InputParser::new();
    parser.feed(b"\x1b[200~abc");
    parser.flush();
    assert_eq!(parser.feed(b"\x01def\x1b[201~"), vec![InputEvent::Paste(b"\x01def".to_vec())]);
    // A paste that came whole is one `Paste`, never an empty one.
    assert_eq!(parser.feed(b"\x1b[200~\x1b[201~"), vec![]);
}

// ---- Parsing: raw fallback and terminal responses ----

#[test]
fn unrecognised_sequences_are_raw() {
    for seq in [
        &b"\x1b[0n"[..],
        b"\x1b[?62;22c",
        b"\x1b[>1;10;0c",
        b"\x1b[12;40R",
        b"\x1b[1;40R",
        b"\x1b[99~",
        b"\x1b[2;99~",
        b"\x1b[201~",
        b"\x1b[5A",
        b"\x1b[?1;2$y",
        b"\x1b[1 q",
    ] {
        assert_eq!(parse_all(seq), vec![raw_ev(seq)], "{seq:?}");
    }
    assert_eq!(parse_all(b"\xff"), vec![raw_ev(b"\xff")]);
    assert_eq!(parse_all(b"\x80"), vec![raw_ev(b"\x80")]);
    assert_eq!(parse_all(b"\xc3("), vec![raw_ev(b"\xc3"), key_ev(Char('('), NONE, b"(")]);
    assert_eq!(parse_all(b"\x1b\x80"), vec![key_ev(Esc, NONE, b"\x1b"), raw_ev(b"\x80")]);
    assert_eq!(parse_all(b"\x1b[12\x1b[A"), vec![raw_ev(b"\x1b[12"), key_ev(Up, NONE, b"\x1b[A")]);
    assert_eq!(parse_all(b"\x1b[1;2\x01"), vec![raw_ev(b"\x1b[1;2"), key_ev(Char('a'), CTRL, b"\x01")]);
    assert_eq!(parse_all(b"\x1b[\x01"), vec![key_ev(Char('['), ALT, b"\x1b["), key_ev(Char('a'), CTRL, b"\x01")]);
    let mut long = b"\x1b[".to_vec();
    long.extend(std::iter::repeat_n(b'1', 300));
    long.push(b'A');
    let events = parse_all(&long);
    assert_eq!(events[0], raw_ev(&long[..MAX_CSI_LEN]));
}

#[test]
fn string_sequences_are_raw() {
    for seq in [
        &b"\x1b]11;rgb:1a1a/2b2b/3c3c\x07"[..],
        b"\x1b]11;rgb:1a1a/2b2b/3c3c\x1b\\",
        b"\x1b]52;c;aGVsbG8=\x1b\\",
        b"\x1bP>|ghostty 1.2.0\x1b\\",
        b"\x1bP1$r0m\x1b\\",
        b"\x1b_Gi=1;OK\x1b\\",
        b"\x1b^private\x1b\\",
        b"\x1bXsos\x07",
        b"\x1b]10;?\x1b\\",
    ] {
        assert_eq!(parse_all(seq), vec![raw_ev(seq)], "{seq:?}");
    }
    // Followed by keys, and a string aborted by another sequence.
    assert_eq!(
        parse_all(b"\x1b]11;x\x07ab"),
        vec![raw_ev(b"\x1b]11;x\x07"), key_ev(Char('a'), NONE, b"a"), key_ev(Char('b'), NONE, b"b")]
    );
    assert_eq!(parse_all(b"\x1b]11;x\x1b[A"), vec![raw_ev(b"\x1b]11;x"), key_ev(Up, NONE, b"\x1b[A")]);
}

#[test]
fn osc_split_at_every_offset() {
    let data = b"a\x1b]11;rgb:0000/1111/2222\x1b\\\x1bP>|kitty(0.40)\x1b\\\x1b]10;rgb:f/f/f\x07b";
    let whole = parse_all(data);
    assert_eq!(whole.len(), 5);
    for i in 0..=data.len() {
        // Splitting right after an introducing ESC triggers the lone ESC rule.
        if i > 0 && data[i - 1] == ESC && data.get(i) != Some(&b'\\') {
            continue;
        }
        assert_eq!(feed_chunks(&[&data[..i], &data[i..]]), whole, "split at {i}");
    }
    // Byte by byte after the introducer.
    let mut parser = InputParser::new();
    let mut events = parser.feed(b"\x1b]");
    for b in b"11;rgb:0/0/0\x1b\\" {
        events.extend(parser.feed(&[*b]));
    }
    assert_eq!(events, vec![raw_ev(b"\x1b]11;rgb:0/0/0\x1b\\")]);
}

#[test]
fn flush_resolves_partial_sequences() {
    let cases: &[(&[u8], InputEvent)] = &[
        (b"\x1b[", key_ev(Char('['), ALT, b"\x1b[")),
        (b"\x1bO", key_ev(Char('O'), mods("sa"), b"\x1bO")),
        (b"\x1b]", key_ev(Char(']'), ALT, b"\x1b]")),
        (b"\x1bP", key_ev(Char('P'), mods("sa"), b"\x1bP")),
        (b"\x1b_", key_ev(Char('_'), ALT, b"\x1b_")),
        (b"\x1b^", key_ev(Char('^'), ALT, b"\x1b^")),
        (b"\x1bX", key_ev(Char('X'), mods("sa"), b"\x1bX")),
        (b"\x1b[1;", raw_ev(b"\x1b[1;")),
        (b"\x1b[<0;1", raw_ev(b"\x1b[<0;1")),
        (b"\x1b]11;rgb:0", raw_ev(b"\x1b]11;rgb:0")),
        (b"\xe2\x82", raw_ev(b"\xe2\x82")),
        (b"\x1b\x1b[", raw_ev(b"\x1b\x1b[")),
    ];
    for (seq, expected) in cases {
        let mut parser = InputParser::new();
        assert!(parser.feed(seq).is_empty(), "{seq:?}");
        assert!(parser.has_pending());
        assert_eq!(parser.flush(), vec![expected.clone()], "{seq:?}");
        assert!(!parser.has_pending());
        assert!(parser.flush().is_empty());
    }
    // A pending prefix completed by the next chunk is not flushed.
    let mut parser = InputParser::new();
    assert!(parser.feed(b"\x1b[1;5").is_empty());
    assert_eq!(parser.feed(b"A"), vec![key_ev(Up, CTRL, b"\x1b[1;5A")]);
}

// ---- Parsing: chunk splitting ----

/// A mixed input and the offsets of the ESCs that start a sequence outside a paste.
fn mixed_input() -> (Vec<u8>, Vec<usize>) {
    let parts: &[&[u8]] = &[
        b"ab",
        "é😀".as_bytes(),
        b"\x01",
        b"\x1b[A",
        b"\x1b[1;5C",
        b"\x1bOP",
        b"\x1b[15;2~",
        b"\x1bh",
        b"\x1b\x18",
        b"\x1b\x1b[1;5A",
        b"\x1b[<0;10;5M",
        b"\x1b[<32;11;5M",
        b"\x1b[<0;11;5m",
        b"\x1b[<65;1;1M",
        b"\x1b[M\x20\x21\x21",
        b"\x1b[M\x23\x21\x21",
        b"\x1b[200~",
        "paste\nwith \x1b[A esc and ünïcode".as_bytes(),
        b"\x1b[201~",
        b"\x1b[I",
        b"\x1b[97;5u",
        b"\x1b]11;rgb:0000/0000/0000\x1b\\",
        b"\x1b[0n",
        b"\x1b[Z",
        "z日".as_bytes(),
    ];
    let mut data = Vec::new();
    let mut esc_starts = Vec::new();
    for part in parts {
        if part[0] == ESC {
            esc_starts.push(data.len());
        }
        data.extend_from_slice(part);
    }
    (data, esc_starts)
}

#[test]
fn mixed_input_events() {
    use MouseButton as B;
    use MouseKind as K;
    let (data, _) = mixed_input();
    assert_eq!(
        parse_all(&data),
        vec![
            key_ev(Char('a'), NONE, b"a"),
            key_ev(Char('b'), NONE, b"b"),
            key_ev(Char('é'), NONE, "é".as_bytes()),
            key_ev(Char('😀'), NONE, "😀".as_bytes()),
            key_ev(Char('a'), CTRL, b"\x01"),
            key_ev(Up, NONE, b"\x1b[A"),
            key_ev(Right, CTRL, b"\x1b[1;5C"),
            key_ev(F(1), NONE, b"\x1bOP"),
            key_ev(F(5), SHIFT, b"\x1b[15;2~"),
            key_ev(Char('h'), ALT, b"\x1bh"),
            key_ev(Char('x'), mods("ac"), b"\x1b\x18"),
            key_ev(Up, mods("ac"), b"\x1b\x1b[1;5A"),
            mouse_ev(K::Press(B::Left), 9, 4, NONE),
            mouse_ev(K::Drag(B::Left), 10, 4, NONE),
            mouse_ev(K::Release(B::Left), 10, 4, NONE),
            mouse_ev(K::ScrollDown, 0, 0, NONE),
            mouse_ev(K::Press(B::Left), 0, 0, NONE),
            mouse_ev(K::Release(B::Left), 0, 0, NONE),
            InputEvent::Paste("paste\nwith \x1b[A esc and ünïcode".as_bytes().to_vec()),
            InputEvent::FocusGained,
            key_ev(Char('a'), CTRL, b"\x1b[97;5u"),
            raw_ev(b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
            raw_ev(b"\x1b[0n"),
            key_ev(BackTab, SHIFT, b"\x1b[Z"),
            key_ev(Char('z'), NONE, b"z"),
            key_ev(Char('日'), NONE, "日".as_bytes()),
        ]
    );
}

#[test]
fn split_at_every_offset_but_after_a_lone_esc_matches_single_chunk() {
    let (data, esc_starts) = mixed_input();
    let lone_esc = |i: usize| i > 0 && esc_starts.contains(&(i - 1));
    let whole = parse_all(&data);
    for i in 0..=data.len() {
        if !lone_esc(i) {
            assert_eq!(feed_chunks(&[&data[..i], &data[i..]]), whole, "split at {i}");
        }
    }
}

#[test]
fn split_into_three_chunks_but_after_a_lone_esc_matches_single_chunk() {
    let (data, esc_starts) = mixed_input();
    let lone_esc = |i: usize| i > 0 && esc_starts.contains(&(i - 1));
    let whole = parse_all(&data);
    for i in (0..=data.len()).filter(|&i| !lone_esc(i)) {
        for j in (i..=data.len()).filter(|&j| !lone_esc(j)) {
            assert_eq!(feed_chunks(&[&data[..i], &data[i..j], &data[j..]]), whole, "split at {i}, {j}");
        }
    }
}

// ---- Encoding keys ----

fn app_cursor() -> PaneInputModes {
    PaneInputModes { app_cursor: true, ..Default::default() }
}

#[test]
fn encode_key_passes_raw_through() {
    for seq in [
        &b"a"[..],
        "é".as_bytes(),
        b"\x01",
        b"\x7f",
        b"\x1b",
        b"\x1b[1;5A",
        b"\x1b[15~",
        b"\x1bOP",
        b"\x1bh",
        b"\x1b\x18",
        b"\x1b[Z",
        b"\x1b[3;2~",
        b"\x1b\x1b[A",
    ] {
        let key = parse_key(seq);
        assert_eq!(encode_key(&key, PaneInputModes::default()), seq, "{seq:?}");
        assert_eq!(encode_key(&key, app_cursor()), seq, "{seq:?}");
    }
}

#[test]
fn encode_key_cursor_mode() {
    let normal = PaneInputModes::default();
    let cases: &[(&[u8], &[u8], &[u8])] = &[
        (b"\x1b[A", b"\x1b[A", b"\x1bOA"),
        (b"\x1b[B", b"\x1b[B", b"\x1bOB"),
        (b"\x1b[C", b"\x1b[C", b"\x1bOC"),
        (b"\x1b[D", b"\x1b[D", b"\x1bOD"),
        (b"\x1bOA", b"\x1b[A", b"\x1bOA"),
        (b"\x1b[H", b"\x1b[H", b"\x1bOH"),
        (b"\x1b[F", b"\x1b[F", b"\x1bOF"),
        (b"\x1b[1~", b"\x1b[H", b"\x1bOH"),
        (b"\x1b[4~", b"\x1b[F", b"\x1bOF"),
        (b"\x1b[1;5A", b"\x1b[1;5A", b"\x1b[1;5A"),
        (b"\x1b[1;2H", b"\x1b[1;2H", b"\x1b[1;2H"),
    ];
    for &(seq, plain, app) in cases {
        let key = parse_key(seq);
        assert_eq!(encode_key(&key, normal), plain, "{seq:?}");
        assert_eq!(encode_key(&key, app_cursor()), app, "{seq:?}");
    }
}

#[test]
fn encode_key_reencodes_extended_keys() {
    let modes = PaneInputModes::default();
    for (seq, expected) in [
        (&b"\x1b[97;5u"[..], &b"\x01"[..]),
        (b"\x1b[97;2u", b"A"),
        (b"\x1b[97;3u", b"\x1ba"),
        (b"\x1b[13;3u", b"\x1b\r"),
        (b"\x1b[9;2u", b"\x1b[Z"),
        (b"\x1b[127;3u", b"\x1b\x7f"),
        (b"\x1b[127;5u", b"\x08"),
        (b"\x1b[27u", b"\x1b"),
        (b"\x1b[32;5u", b"\x00"),
        (b"\x1b[27;5;97~", b"\x01"),
        (b"\x1b\x1b[97;5u", b"\x1b\x01"),
    ] {
        assert_eq!(encode_key(&parse_key(seq), modes), expected, "{seq:?}");
    }
    assert_eq!(encode_key(&Key::new(Up, NONE), app_cursor()), b"\x1bOA");
    assert_eq!(encode_key(&Key::new(Char('c'), CTRL), modes), b"\x03");
}

#[test]
fn encode_key_keypad() {
    let keypad = PaneInputModes { app_keypad: true, ..Default::default() };
    let normal = PaneInputModes::default();
    assert_eq!(encode_key(&parse_key(b"\x1bOM"), normal), b"\r");
    assert_eq!(encode_key(&parse_key(b"\x1bOM"), keypad), b"\x1bOM");
    assert_eq!(encode_key(&parse_key(b"\x1bOp"), normal), b"0");
    assert_eq!(encode_key(&parse_key(b"\x1bOp"), keypad), b"\x1bOp");
}

#[test]
fn encode_key_code_table() {
    let n = PaneInputModes::default();
    let a = app_cursor();
    let cases: &[(KeyCode, Mods, PaneInputModes, &[u8])] = &[
        (Char('a'), NONE, n, b"a"),
        (Char('a'), SHIFT, n, b"A"),
        (Char('A'), NONE, n, b"A"),
        (Char('a'), CTRL, n, b"\x01"),
        (Char('A'), CTRL, n, b"\x01"),
        (Char('z'), CTRL, n, b"\x1a"),
        (Char(' '), CTRL, n, b"\x00"),
        (Char('@'), CTRL, n, b"\x00"),
        (Char('['), CTRL, n, b"\x1b"),
        (Char('\\'), CTRL, n, b"\x1c"),
        (Char(']'), CTRL, n, b"\x1d"),
        (Char('^'), CTRL, n, b"\x1e"),
        (Char('_'), CTRL, n, b"\x1f"),
        (Char('?'), CTRL, n, b"\x7f"),
        (Char('1'), CTRL, n, b"1"),
        (Char('a'), ALT, n, b"\x1ba"),
        (Char('x'), mods("ac"), n, b"\x1b\x18"),
        (Char('é'), NONE, n, "é".as_bytes()),
        (Char('é'), ALT, n, "\x1bé".as_bytes()),
        (Enter, NONE, n, b"\r"),
        (Enter, ALT, n, b"\x1b\r"),
        (Enter, CTRL, n, b"\r"),
        (Tab, NONE, n, b"\t"),
        (Tab, ALT, n, b"\x1b\t"),
        (Tab, SHIFT, n, b"\x1b[Z"),
        (BackTab, NONE, n, b"\x1b[Z"),
        (BackTab, SHIFT, n, b"\x1b[Z"),
        (BackTab, CTRL, n, b"\x1b[1;6Z"),
        (BackTab, ALT, n, b"\x1b[1;4Z"),
        (Backspace, NONE, n, b"\x7f"),
        (Backspace, CTRL, n, b"\x08"),
        (Backspace, ALT, n, b"\x1b\x7f"),
        (Esc, NONE, n, b"\x1b"),
        (Esc, ALT, n, b"\x1b\x1b"),
        (Up, NONE, n, b"\x1b[A"),
        (Up, NONE, a, b"\x1bOA"),
        (Down, NONE, n, b"\x1b[B"),
        (Right, NONE, a, b"\x1bOC"),
        (Up, CTRL, n, b"\x1b[1;5A"),
        (Up, CTRL, a, b"\x1b[1;5A"),
        (Left, mods("sa"), n, b"\x1b[1;4D"),
        (Home, NONE, n, b"\x1b[H"),
        (Home, NONE, a, b"\x1bOH"),
        (End, NONE, a, b"\x1bOF"),
        (End, SHIFT, n, b"\x1b[1;2F"),
        (Insert, NONE, n, b"\x1b[2~"),
        (Delete, NONE, n, b"\x1b[3~"),
        (Delete, SHIFT, n, b"\x1b[3;2~"),
        (PageUp, NONE, n, b"\x1b[5~"),
        (PageDown, CTRL, n, b"\x1b[6;5~"),
        (F(1), NONE, n, b"\x1bOP"),
        (F(4), NONE, a, b"\x1bOS"),
        (F(1), SHIFT, n, b"\x1b[1;2P"),
        (F(5), NONE, n, b"\x1b[15~"),
        (F(6), NONE, n, b"\x1b[17~"),
        (F(10), NONE, n, b"\x1b[21~"),
        (F(11), NONE, n, b"\x1b[23~"),
        (F(12), NONE, n, b"\x1b[24~"),
        (F(12), CTRL, n, b"\x1b[24;5~"),
        (F(20), NONE, n, b"\x1b[34~"),
        (F(0), NONE, n, b""),
        (F(21), NONE, n, b""),
    ];
    for &(code, m, modes, expected) in cases {
        assert_eq!(encode_key_code(code, m, modes), expected, "{code:?} {m:?}");
    }
}

#[test]
fn encode_key_code_round_trips_through_parser() {
    let all_mods: Vec<Mods> = (0u32..8).map(|b| Mods::from_xterm_param(b + 1)).collect();
    let mut codes = vec![Up, Down, Left, Right, Home, End, Insert, Delete, PageUp, PageDown];
    codes.extend((1..=20).map(F));
    for code in codes {
        for &m in &all_mods {
            for modes in [PaneInputModes::default(), app_cursor()] {
                let bytes = encode_key_code(code, m, modes);
                assert_eq!(single_key(&bytes), (code, m), "{code:?} {m:?}");
            }
        }
    }
    for c in ('a'..='z').chain(['A', '1', '%', 'é']) {
        for m in [NONE, ALT] {
            let bytes = encode_key_code(Char(c), m, PaneInputModes::default());
            let (code, mods) = single_key(&bytes);
            assert_eq!(code, Char(c));
            assert_eq!(mods.alt, m.alt);
        }
    }
    for c in 'a'..='z' {
        if c == 'i' || c == 'm' {
            continue; // Tab and Enter
        }
        for m in [CTRL, mods("ac")] {
            let bytes = encode_key_code(Char(c), m, PaneInputModes::default());
            assert_eq!(single_key(&bytes), (Char(c), m), "{c} {m:?}");
        }
    }
}

// ---- Encoding mouse ----

fn ev(kind: MouseKind, m: Mods) -> MouseEvent {
    MouseEvent { kind, col: 100, row: 100, mods: m }
}

#[test]
fn encode_mouse_sgr() {
    use MouseButton as B;
    use MouseKind as K;
    let modes = PaneInputModes { mouse_motion: true, sgr_mouse: true, ..Default::default() };
    let cases: &[(MouseKind, Mods, &[u8])] = &[
        (K::Press(B::Left), NONE, b"\x1b[<0;3;5M"),
        (K::Press(B::Middle), NONE, b"\x1b[<1;3;5M"),
        (K::Press(B::Right), NONE, b"\x1b[<2;3;5M"),
        (K::Release(B::Left), NONE, b"\x1b[<0;3;5m"),
        (K::Release(B::Right), NONE, b"\x1b[<2;3;5m"),
        (K::Drag(B::Left), NONE, b"\x1b[<32;3;5M"),
        (K::Drag(B::Right), NONE, b"\x1b[<34;3;5M"),
        (K::Move, NONE, b"\x1b[<35;3;5M"),
        (K::ScrollUp, NONE, b"\x1b[<64;3;5M"),
        (K::ScrollDown, NONE, b"\x1b[<65;3;5M"),
        (K::ScrollLeft, NONE, b"\x1b[<66;3;5M"),
        (K::ScrollRight, NONE, b"\x1b[<67;3;5M"),
        (K::Press(B::Left), SHIFT, b"\x1b[<4;3;5M"),
        (K::Press(B::Left), ALT, b"\x1b[<8;3;5M"),
        (K::Press(B::Left), CTRL, b"\x1b[<16;3;5M"),
        (K::ScrollUp, mods("sc"), b"\x1b[<84;3;5M"),
    ];
    for &(kind, m, expected) in cases {
        assert_eq!(encode_mouse(&ev(kind, m), 2, 4, modes).unwrap(), expected, "{kind:?}");
    }
    // Large coordinates are fine in SGR.
    assert_eq!(encode_mouse(&ev(K::Press(B::Left), NONE), 999, 499, modes).unwrap(), b"\x1b[<0;1000;500M");
    // Round trip through the parser.
    for &(kind, m, _) in cases {
        let bytes = encode_mouse(&ev(kind, m), 7, 9, modes).unwrap();
        assert_eq!(single_mouse(&bytes), MouseEvent { kind, col: 7, row: 9, mods: m });
    }
}

#[test]
fn encode_mouse_filters_by_mode() {
    use MouseButton as B;
    use MouseKind as K;
    let none = PaneInputModes { sgr_mouse: true, ..Default::default() };
    let click = PaneInputModes { mouse_click: true, ..none };
    let drag = PaneInputModes { mouse_drag: true, ..none };
    let motion = PaneInputModes { mouse_motion: true, ..none };
    let press = ev(K::Press(B::Left), NONE);
    let release = ev(K::Release(B::Left), NONE);
    let dragged = ev(K::Drag(B::Left), NONE);
    let moved = ev(K::Move, NONE);
    let wheel = ev(K::ScrollUp, NONE);
    for e in [&press, &release, &dragged, &moved, &wheel] {
        assert_eq!(encode_mouse(e, 0, 0, none), None);
    }
    for e in [&press, &release, &wheel] {
        assert!(encode_mouse(e, 0, 0, click).is_some());
        assert!(encode_mouse(e, 0, 0, drag).is_some());
        assert!(encode_mouse(e, 0, 0, motion).is_some());
    }
    assert_eq!(encode_mouse(&dragged, 0, 0, click), None);
    assert!(encode_mouse(&dragged, 0, 0, drag).is_some());
    assert!(encode_mouse(&dragged, 0, 0, motion).is_some());
    assert_eq!(encode_mouse(&moved, 0, 0, click), None);
    assert_eq!(encode_mouse(&moved, 0, 0, drag), None);
    assert!(encode_mouse(&moved, 0, 0, motion).is_some());
}

#[test]
fn encode_mouse_x10() {
    use MouseButton as B;
    use MouseKind as K;
    let modes = PaneInputModes { mouse_drag: true, ..Default::default() };
    let enc = |kind, m, col, row| encode_mouse(&ev(kind, m), col, row, modes);
    assert_eq!(enc(K::Press(B::Left), NONE, 0, 0).unwrap(), b"\x1b[M\x20\x21\x21");
    assert_eq!(enc(K::Press(B::Right), NONE, 4, 2).unwrap(), b"\x1b[M\x22\x25\x23");
    assert_eq!(enc(K::Release(B::Right), NONE, 4, 2).unwrap(), b"\x1b[M\x23\x25\x23");
    assert_eq!(enc(K::Release(B::Left), CTRL, 0, 0).unwrap(), b"\x1b[M\x33\x21\x21");
    assert_eq!(enc(K::Drag(B::Left), NONE, 0, 0).unwrap(), b"\x1b[M\x40\x21\x21");
    assert_eq!(enc(K::ScrollDown, NONE, 0, 0).unwrap(), b"\x1b[M\x61\x21\x21");
    assert_eq!(enc(K::Press(B::Left), NONE, 222, 222).unwrap(), b"\x1b[M\x20\xff\xff");
    assert_eq!(enc(K::Press(B::Left), NONE, 223, 0), None);
    assert_eq!(enc(K::Press(B::Left), NONE, 0, 223), None);
    // Parsed back (release resolved from the press).
    let mut parser = InputParser::new();
    let mut bytes = enc(K::Press(B::Right), NONE, 4, 2).unwrap();
    bytes.extend(enc(K::Release(B::Right), NONE, 4, 2).unwrap());
    assert_eq!(
        parser.feed(&bytes),
        vec![mouse_ev(K::Press(B::Right), 4, 2, NONE), mouse_ev(K::Release(B::Right), 4, 2, NONE),]
    );
}

#[test]
fn encode_mouse_utf8() {
    use MouseButton as B;
    use MouseKind as K;
    let modes = PaneInputModes { mouse_click: true, utf8_mouse: true, ..Default::default() };
    let enc = |col, row| encode_mouse(&ev(K::Press(B::Left), NONE), col, row, modes);
    assert_eq!(enc(0, 0).unwrap(), b"\x1b[M\x20\x21\x21");
    // 300 + 1 + 32 = 333 = U+014D.
    assert_eq!(enc(300, 0).unwrap(), "\x1b[M\u{20}\u{14d}\u{21}".as_bytes());
    assert!(enc(2014, 2014).is_some());
    assert_eq!(enc(2015, 0), None);
    // SGR wins over UTF-8.
    let both = PaneInputModes { sgr_mouse: true, ..modes };
    assert_eq!(encode_mouse(&ev(K::Press(B::Left), NONE), 300, 0, both).unwrap(), b"\x1b[<0;301;1M");
}

#[test]
fn alternate_scroll() {
    let modes = PaneInputModes { alternate_scroll: true, alt_screen: true, ..Default::default() };
    let up = ev(MouseKind::ScrollUp, NONE);
    let down = ev(MouseKind::ScrollDown, NONE);
    assert_eq!(encode_alternate_scroll(&up, modes).unwrap(), b"\x1b[A");
    assert_eq!(encode_alternate_scroll(&down, modes).unwrap(), b"\x1b[B");
    let app = PaneInputModes { app_cursor: true, ..modes };
    assert_eq!(encode_alternate_scroll(&up, app).unwrap(), b"\x1bOA");
    assert_eq!(encode_alternate_scroll(&ev(MouseKind::ScrollLeft, NONE), modes), None);
    let primary = PaneInputModes { alt_screen: false, ..modes };
    assert_eq!(encode_alternate_scroll(&up, primary), None);
    let mouse = PaneInputModes { mouse_click: true, ..modes };
    assert_eq!(encode_alternate_scroll(&up, mouse), None);
}

// ---- Encoding paste ----

#[test]
fn encode_paste_bracketed() {
    let modes = PaneInputModes { bracketed_paste: true, ..Default::default() };
    assert_eq!(encode_paste(b"hi\nthere", modes), b"\x1b[200~hi\nthere\x1b[201~");
    assert_eq!(encode_paste(b"a\x1b[201~b", modes), b"\x1b[200~ab\x1b[201~");
    // Removing a marker must not leave a new one behind.
    assert_eq!(encode_paste(b"\x1b[20\x1b[201~1~x", modes), b"\x1b[200~x\x1b[201~");
    assert_eq!(encode_paste(b"", modes), b"\x1b[200~\x1b[201~");
}

#[test]
fn encode_paste_plain() {
    let modes = PaneInputModes::default();
    assert_eq!(encode_paste(b"a\nb\r\nc\rd", modes), b"a\rb\rc\rd");
    assert_eq!(encode_paste(b"\n\n", modes), b"\r\r");
    assert_eq!(encode_paste(b"x\x1b[201~", modes), b"x\x1b[201~");
}

// ---- kitty keyboard protocol ----

fn modes(flags: u8) -> PaneInputModes {
    PaneInputModes { kitty_flags: flags, ..Default::default() }
}

#[test]
fn shift_enter_reaches_kitty_panes_only() {
    let mut p = InputParser::new();
    let ev = p.feed(b"\x1b[13;2u");
    let InputEvent::Key(k) = &ev[0] else { panic!("{ev:?}") };
    assert_eq!((k.code, k.mods.shift), (KeyCode::Enter, true));
    assert_eq!(encode_key(k, modes(1)), b"\x1b[13;2u");
    assert_eq!(encode_key(k, modes(0)), b"\r");
}

#[test]
fn kitty_encodings() {
    let ctrl = Mods { ctrl: true, ..Default::default() };
    let alt = Mods { alt: true, ..Default::default() };
    assert_eq!(encode_kitty(KeyCode::Char('c'), ctrl, 1).unwrap(), b"\x1b[99;5u");
    assert_eq!(encode_kitty(KeyCode::Char('H'), alt, 1).unwrap(), b"\x1b[104;4u");
    assert_eq!(encode_kitty(KeyCode::Esc, Mods::default(), 1).unwrap(), b"\x1b[27u");
    assert_eq!(encode_kitty(KeyCode::Char('a'), Mods::default(), 1), None, "plain text stays text");
    assert_eq!(encode_kitty(KeyCode::Char('a'), Mods::default(), 8).unwrap(), b"\x1b[97u");
    assert_eq!(encode_kitty(KeyCode::Enter, Mods::default(), 1), None);
    assert_eq!(encode_kitty(KeyCode::Up, ctrl, 1), None, "arrows keep their CSI form");
    // Ctrl+C reaches a legacy pane as ETX even when received as CSI u.
    let mut p = InputParser::new();
    let ev = p.feed(b"\x1b[99;5u");
    let InputEvent::Key(k) = &ev[0] else { panic!() };
    assert_eq!(encode_key(k, modes(0)), b"\x03");
}
// ---- KeySpec ----

#[test]
fn keyspec_parse() {
    let cases: &[(&str, KeyCode, Mods)] = &[
        ("ctrl+b", Char('b'), CTRL),
        ("Ctrl+B", Char('b'), CTRL),
        ("alt+h", Char('h'), ALT),
        ("alt+H", Char('H'), ALT),
        ("alt+shift+h", Char('H'), ALT),
        ("alt+shift+left", Left, mods("sa")),
        ("%", Char('%'), NONE),
        ("|", Char('|'), NONE),
        ("?", Char('?'), NONE),
        ("N", Char('N'), NONE),
        ("shift+n", Char('N'), NONE),
        ("n", Char('n'), NONE),
        ("ctrl+shift+a", Char('a'), CTRL),
        ("shift+tab", BackTab, SHIFT),
        ("backtab", BackTab, SHIFT),
        ("ctrl+shift+tab", BackTab, mods("sc")),
        ("tab", Tab, NONE),
        ("f5", F(5), NONE),
        ("F12", F(12), NONE),
        ("ctrl+f1", F(1), CTRL),
        ("enter", Enter, NONE),
        ("Return", Enter, NONE),
        ("space", Char(' '), NONE),
        ("ctrl+space", Char(' '), CTRL),
        ("ctrl+@", Char('@'), CTRL),
        ("esc", Esc, NONE),
        ("Escape", Esc, NONE),
        ("pageup", PageUp, NONE),
        ("PgDn", PageDown, NONE),
        ("alt+[", Char('['), ALT),
        ("+", Char('+'), NONE),
        ("ctrl++", Char('+'), CTRL),
        ("alt+plus", Char('+'), ALT),
        ("backspace", Backspace, NONE),
        ("ctrl+alt+Delete", Delete, mods("ac")),
        ("meta+x", Char('x'), ALT),
        (" control + x ", Char('x'), CTRL),
        ("é", Char('é'), NONE),
    ];
    for &(s, code, m) in cases {
        assert_eq!(KeySpec::parse(s).unwrap(), KeySpec { code, mods: m }, "{s:?}");
        assert_eq!(s.parse::<KeySpec>().unwrap(), KeySpec { code, mods: m }, "{s:?}");
    }
}

#[test]
fn keyspec_parse_errors() {
    for s in ["", "ctrl+", "hyper+a", "f0", "f21", "foo", "ctrl+ab", "ctrl++b", "a++"] {
        let err = KeySpec::parse(s).expect_err(s);
        assert!(err.to_string().contains("invalid key"), "{err}");
    }
}

#[test]
fn keyspec_display_round_trip() {
    let cases = [
        ("ctrl+b", "ctrl+b"),
        ("Ctrl+B", "ctrl+b"),
        ("alt+shift+left", "alt+shift+left"),
        ("shift+alt+ctrl+up", "ctrl+alt+shift+up"),
        ("ALT+SHIFT+H", "alt+H"),
        ("shift+n", "N"),
        ("%", "%"),
        ("shift+tab", "shift+tab"),
        ("backtab", "shift+tab"),
        ("ctrl+shift+tab", "ctrl+shift+tab"),
        ("F5", "f5"),
        ("Return", "enter"),
        ("space", "space"),
        ("ctrl+space", "ctrl+space"),
        ("esc", "esc"),
        ("PgUp", "pageup"),
        ("pgdn", "pagedown"),
        ("ins", "insert"),
        ("del", "delete"),
        ("bs", "backspace"),
        ("alt+[", "alt+["),
        ("+", "+"),
        ("ctrl++", "ctrl++"),
        ("home", "home"),
        ("end", "end"),
        ("down", "down"),
        ("right", "right"),
        ("f20", "f20"),
    ];
    for (input, display) in cases {
        let spec = KeySpec::parse(input).unwrap();
        assert_eq!(spec.to_string(), display, "{input:?}");
        assert_eq!(KeySpec::parse(display).unwrap(), spec, "{display:?}");
    }
}

#[test]
fn keyspec_matches() {
    let yes = |spec: &str, bytes: &[u8]| {
        assert!(KeySpec::parse(spec).unwrap().matches(&parse_key(bytes)), "{spec:?} vs {bytes:?}");
    };
    let no = |spec: &str, bytes: &[u8]| {
        assert!(!KeySpec::parse(spec).unwrap().matches(&parse_key(bytes)), "{spec:?} vs {bytes:?}");
    };
    yes("%", b"%");
    yes("%", b"\x1b[37;2u");
    yes("|", b"|");
    yes("N", b"N");
    yes("shift+n", b"N");
    no("N", b"n");
    no("n", b"N");
    yes("alt+H", b"\x1bH");
    yes("alt+shift+h", b"\x1bH");
    no("alt+H", b"\x1bh");
    no("alt+H", b"H");
    yes("ctrl+b", b"\x02");
    yes("ctrl+B", b"\x02");
    yes("ctrl+b", b"\x1b[98;5u");
    yes("ctrl+b", b"\x1b[66;6u");
    yes("ctrl+shift+b", b"\x02");
    no("ctrl+b", b"b");
    no("ctrl+b", b"\x1b\x02");
    no("b", b"\x02");
    yes("ctrl+alt+x", b"\x1b\x18");
    yes("shift+tab", b"\x1b[Z");
    yes("shift+tab", b"\x1b[9;2u");
    no("tab", b"\x1b[Z");
    yes("tab", b"\t");
    yes("ctrl+i", b"\t");
    yes("tab", b"\x1b[105;5u");
    yes("esc", b"\x1b");
    yes("ctrl+[", b"\x1b");
    yes("esc", b"\x1b[91;5u");
    yes("ctrl+space", b"\x00");
    yes("ctrl+@", b"\x00");
    yes("space", b" ");
    no("space", b"\x00");
    yes("ctrl+h", b"\x08");
    yes("ctrl+backspace", b"\x08");
    yes("backspace", b"\x7f");
    no("backspace", b"\x08");
    yes("alt+backspace", b"\x1b\x7f");
    yes("enter", b"\r");
    yes("enter", b"\x1bOM");
    no("enter", b"\n");
    yes("ctrl+j", b"\n");
    yes("alt+shift+left", b"\x1b[1;4D");
    no("alt+shift+left", b"\x1b[1;3D");
    no("left", b"\x1b[1;2D");
    yes("up", b"\x1bOA");
    yes("alt+up", b"\x1b\x1b[A");
    yes("alt+up", b"\x1b[1;3A");
    yes("f5", b"\x1b[15~");
    no("f5", b"\x1b[15;2~");
    yes("ctrl+f5", b"\x1b[15;5~");
    yes("pageup", b"\x1b[5~");

    let mut parser = InputParser::new();
    parser.feed(b"\x1b[");
    let flushed = parser.flush();
    let [InputEvent::Key(key)] = flushed.as_slice() else { panic!("{flushed:?}") };
    assert!(KeySpec::parse("alt+[").unwrap().matches(key));
    assert!(KeySpec::parse("q").unwrap().matches(&Key::new(Char('q'), NONE)));
}

#[test]
fn a_paste_that_never_ends_stays_bounded() {
    let mut parser = InputParser::new();
    assert!(parser.feed(b"\x1b[200~").is_empty());
    let chunk = vec![b'x'; 64 << 10];
    let mut pasted = 0;
    for _ in 0..64 {
        for ev in parser.feed(&chunk) {
            match ev {
                InputEvent::PastePiece(data) => pasted += data.len(),
                other => panic!("{other:?}"),
            }
        }
    }
    // 4 MiB sent without an end marker: it went out in pieces as it came.
    assert!(pasted >= 3 << 20, "{pasted}");
    // The end marker ends it with what was left.
    let rest = (4 << 20) - pasted;
    assert_eq!(parser.feed(b"\x1b[201~"), vec![InputEvent::Paste(vec![b'x'; rest])]);
}

#[test]
fn lock_bits_do_not_reach_programs() {
    let modes = PaneInputModes::default();
    let key = |bytes: &[u8]| parse_key(bytes);
    assert_eq!(encode_key(&key(b"\x1b[5;129~"), modes), b"\x1b[5~");
    assert_eq!(encode_key(&key(b"\x1b[1;133D"), modes), b"\x1b[1;5D");
    assert_eq!(encode_key(&key(b"\x1b[1;129A"), modes), b"\x1b[A");
    // Behind an Alt prefix too: Alt+PageUp with Num Lock is Alt+PageUp.
    let alt_page_up = key(b"\x1b\x1b[5;129~");
    assert_eq!((alt_page_up.code, alt_page_up.mods), (PageUp, ALT));
    assert_eq!(encode_key(&alt_page_up, modes), b"\x1b[5;3~");
    // Without lock bits, what came is what goes.
    assert_eq!(encode_key(&key(b"\x1b[1;5D"), modes), b"\x1b[1;5D");
}

#[test]
fn a_line_end_split_between_paste_pieces_is_one_line_end() {
    let modes = PaneInputModes::default();
    let first = encode_paste_after(b"one\r", modes, false);
    let second = encode_paste_after(b"\ntwo", modes, true);
    assert_eq!([first, second].concat(), b"one\rtwo");
    // An LF that does not follow a CR is a line end of its own.
    assert_eq!(encode_paste_after(b"\ntwo", modes, false), b"\rtwo");
}

#[test]
fn synthesised_keys_follow_the_kitty_protocol() {
    let kitty = PaneInputModes { kitty_flags: 1, ..Default::default() };
    let shift_enter = Key { code: Enter, mods: SHIFT, raw: Vec::new() };
    assert_eq!(encode_key(&shift_enter, kitty), b"\x1b[13;2u");
    assert_eq!(encode_key(&shift_enter, PaneInputModes::default()), b"\r");
}

// ---- kitty keyboard protocol, every key with its place and text ----

/// The keys parsed from `bytes`, which must all be keys.
fn keys(bytes: &[u8]) -> Vec<Key> {
    parse_all(bytes)
        .into_iter()
        .map(|ev| match ev {
            InputEvent::Key(k) => k,
            other => panic!("{bytes:?}: {other:?}"),
        })
        .collect()
}

#[test]
fn every_key_with_its_text() {
    // AZERTY: the `&` key, at the place of `1`; `é` at the place of `2`; with Shift, `1`.
    assert_eq!(single_key(b"\x1b[38::49;;38u"), (Char('&'), NONE));
    assert_eq!(single_key(b"\x1b[233::50;;233u"), (Char('é'), NONE));
    assert_eq!(single_key(b"\x1b[38:49:49;2;49u"), (Char('1'), SHIFT));
    // A dead key's accent, AltGr and an input method show only in the text.
    assert_eq!(single_key(b"\x1b[101;;234u"), (Char('ê'), NONE));
    assert_eq!(single_key(b"\x1b[101;;8364u"), (Char('€'), NONE));
    // The macOS Option key types a character: Alt is part of it.
    assert_eq!(single_key(b"\x1b[101;3;233u"), (Char('é'), NONE));
    // Caps Lock and Shift. With both, the letter is lowercase, as typed.
    assert_eq!(single_key(b"\x1b[97;65;65u"), (Char('A'), SHIFT));
    assert_eq!(single_key(b"\x1b[115:83;66;115u"), (Char('s'), NONE));
    assert_eq!(single_key(b"\x1b[1099::115;65;1067u"), (Char('Ы'), SHIFT));
    assert_eq!(single_key(b"\x1b[1099:1067:115;2;1067u"), (Char('Ы'), SHIFT));
    // Plain keys, with or without their text.
    assert_eq!(single_key(b"\x1b[97;;97u"), (Char('a'), NONE));
    assert_eq!(single_key(b"\x1b[97u"), (Char('a'), NONE));
    assert_eq!(single_key(b"\x1b[32;;32u"), (Char(' '), NONE));
    assert_eq!(single_key(b"\x1b[13u"), (Enter, NONE));
    assert_eq!(single_key(b"\x1b[9u"), (Tab, NONE));
    assert_eq!(single_key(b"\x1b[127u"), (Backspace, NONE));
    // A control character is no text.
    assert_eq!(single_key(b"\x1b[13;;13u"), (Enter, NONE));
    // Keys with Ctrl carry no text.
    assert_eq!(single_key(b"\x1b[1080::98;5u"), (Char('и'), CTRL));
    // Text of several characters is typed as it is, one key each.
    let typed = keys(b"\x1b[97;;101:769u");
    assert_eq!(typed.iter().map(|k| k.code).collect::<Vec<_>>(), [Char('e'), Char('\u{301}')]);
    let sent: Vec<u8> = typed.iter().flat_map(|k| encode_key(k, PaneInputModes::default())).collect();
    assert_eq!(sent, "e\u{301}".as_bytes());
    // Keys around it are not taken with it.
    let mixed = keys(b"x\x1b[101;;234uy");
    assert_eq!(mixed.iter().map(|k| k.code).collect::<Vec<_>>(), [Char('x'), Char('ê'), Char('y')]);
}

#[test]
fn keys_by_place() {
    let place = |bytes: &[u8]| parse_key(bytes).by_place().map(|k| (k.code, k.mods));
    // The digit row, without Shift: AZERTY's `&` and `é`.
    assert_eq!(place(b"\x1b[38::49;;38u"), Some((Char('1'), NONE)));
    assert_eq!(place(b"\x1b[233::50;;233u"), Some((Char('2'), NONE)));
    assert_eq!(place(b"\x1b[38::49;5u"), Some((Char('1'), CTRL)));
    // With Shift, the key types its digit: that is what it is.
    assert_eq!(place(b"\x1b[38:49:49;2;49u"), None);
    assert_eq!(place(b"\x1b[49;;49u"), None);
    // A non-Latin layout: the letter at the place, Shift and Ctrl kept.
    assert_eq!(place(b"\x1b[1099::115;;1099u"), Some((Char('s'), NONE)));
    assert_eq!(place(b"\x1b[1099:1067:115;2;1067u"), Some((Char('S'), SHIFT)));
    // Caps Lock alone: the case of what was typed.
    assert_eq!(place(b"\x1b[1099::115;65;1067u"), Some((Char('S'), SHIFT)));
    assert_eq!(place(b"\x1b[1080::98;5u"), Some((Char('b'), CTRL)));
    // A Latin letter is the letter it types, wherever the key is (AZERTY's `a` is at `q`).
    assert_eq!(place(b"\x1b[97::113;;97u"), None);
    // Without the protocol's alternates, keys are what they type.
    assert_eq!(place(b"&"), None);
    assert_eq!(place("и".as_bytes()), None);
    assert_eq!(place(b"\x1b[1080;5u"), None);
}

#[test]
fn every_key_reaches_a_program_without_the_protocol_as_typed() {
    let legacy = PaneInputModes::default();
    for (seq, expected) in [
        (&b"\x1b[38::49;;38u"[..], "&".as_bytes()),
        (b"\x1b[233::50;;233u", "é".as_bytes()),
        (b"\x1b[38:49:49;2;49u", b"1"),
        (b"\x1b[101;;234u", "ê".as_bytes()),
        (b"\x1b[101;;8364u", "€".as_bytes()),
        (b"\x1b[101;3;233u", "é".as_bytes()),
        (b"\x1b[97;65;65u", b"A"),
        (b"\x1b[97;;97u", b"a"),
        (b"\x1b[32;;32u", b" "),
        (b"\x1b[13u", b"\r"),
        (b"\x1b[13;2u", b"\r"),
        (b"\x1b[9u", b"\t"),
        (b"\x1b[127u", b"\x7f"),
        (b"\x1b[27u", b"\x1b"),
        (b"\x1b[99;5u", b"\x03"),
        (b"\x1b[97;3u", b"\x1ba"),
        // Ctrl with a Russian letter is the control character of the letter at its place.
        (b"\x1b[1080::98;5u", b"\x02"),
        (b"\x1b[1080;5u", "и".as_bytes()),
    ] {
        assert_eq!(encode_key(&parse_key(seq), legacy), expected, "{seq:?}");
    }
}

#[test]
fn every_key_reaches_a_program_with_the_protocol_as_it_asked() {
    let pane = |flags: u8| PaneInputModes { kitty_flags: flags, ..Default::default() };
    let cases: &[(&[u8], u8, &[u8])] = &[
        // Disambiguated keys only: text stays text, a key with Ctrl is a sequence.
        (b"\x1b[38::49;;38u", 1, b"&"),
        (b"\x1b[101;;234u", 1, "ê".as_bytes()),
        (b"\x1b[1080::98;5u", 1, b"\x1b[1080;5u"),
        (b"\x1b[13;2u", 1, b"\x1b[13;2u"),
        // With alternates, as the terminal gave them.
        (b"\x1b[1080::98;5u", 5, b"\x1b[1080::98;5u"),
        (b"\x1b[97:65;6u", 5, b"\x1b[97:65;6u"),
        (b"\x1b[38::49;5u", 5, b"\x1b[38::49;5u"),
        // Every key as a sequence: without the text flag, only the key; with it, the text too.
        (b"\x1b[101;;234u", 9, b"\x1b[101u"),
        (b"\x1b[101;;234u", 25, b"\x1b[101;1;234u"),
        (b"\x1b[38::49;;38u", 29, b"\x1b[38::49;1;38u"),
        // Caps Lock and Num Lock stay with kraal, as for every program: Caps Lock's `A` is
        // Shift's, Ctrl+C with Num Lock on is Ctrl+C.
        (b"\x1b[97;65;65u", 25, b"\x1b[97;1;65u"),
        (b"\x1b[99;133u", 1, b"\x1b[99;5u"),
        // Keys that came as plain text are described from what they are.
        (b"a", 9, b"\x1b[97u"),
        (b"a", 25, b"\x1b[97;1;97u"),
        (b"A", 25, b"\x1b[97;2;65u"),
        (b"A", 29, b"\x1b[97:65;2;65u"),
        ("é".as_bytes(), 25, b"\x1b[233;1;233u"),
        (b"\r", 9, b"\x1b[13u"),
        (b"\x02", 25, b"\x1b[98;5u"),
    ];
    for &(seq, flags, expected) in cases {
        assert_eq!(encode_key(&parse_key(seq), pane(flags)), expected, "{seq:?} with flags {flags}");
    }
}

#[test]
fn keys_only_some_programs_asked_for() {
    let event = |bytes: &[u8]| match parse_all(bytes).as_slice() {
        [InputEvent::KittyKey { raw, release }] => kitty_key_flag(raw, *release),
        other => panic!("{bytes:?}: {other:?}"),
    };
    // Shift or Caps Lock pressed alone: only with every key as a sequence.
    assert_eq!(event(b"\x1b[57441;2u"), 8);
    assert_eq!(event(b"\x1b[57358;65u"), 8);
    // A release: only with event types.
    assert_eq!(event(b"\x1b[97;1:3u"), 2);
    // Print Screen: any program with the protocol.
    assert_eq!(event(b"\x1b[57361u"), 0);
}

#[test]
fn a_program_with_the_protocol_gets_the_modifiers_as_sent() {
    let modes = PaneInputModes { kitty_flags: 29, ..Default::default() };
    // The macOS Option key typing `é`: Alt stays in the key the program gets.
    let key = parse_key(b"\x1b[101;3;233u");
    assert_eq!(encode_key(&key, modes), b"\x1b[101;3;233u");
    // Caps Lock alone is left out: no Shift is made up, the text says the case.
    let key = parse_key(b"\x1b[97;65;65u");
    assert_eq!(encode_key(&key, modes), b"\x1b[97;1;65u");
}

#[test]
fn text_from_an_input_method_is_text() {
    // No key, two characters: one piece of text, as a paste, not two key presses.
    assert_eq!(parse_all(b"\x1b[0;;49:97u"), vec![InputEvent::Paste(b"1a".to_vec())]);
}

#[test]
fn caps_lock_reaches_a_program_without_text_as_shift() {
    // Flags 1 and 8 without 16: the program learns the case from Shift.
    let modes = PaneInputModes { kitty_flags: 9, ..Default::default() };
    assert_eq!(encode_key(&parse_key(b"\x1b[97;65;65u"), modes), b"\x1b[97;2u");
    // Caps Lock and Shift together type a lowercase letter: no Shift then.
    assert_eq!(encode_key(&parse_key(b"\x1b[115:83;66;115u"), modes), b"\x1b[115u");
}
