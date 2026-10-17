//! A filter on pane output before the emulator: keeps the kitty keyboard mode stack bounded.
//!
//! alacritty_terminal 0.26 evicts from the wrong stack when a program pushes more than 4096
//! keyboard modes (`CSI > flags u`): it removes from the title stack, panicking when that one is
//! empty, which would take the whole server down. The guard runs the emulator's own parser
//! (the same vte version, fed the same bytes) one step ahead of it, so it reads every sequence
//! exactly as the emulator will, controls and odd bytes included. It follows the depth of the
//! two stacks (the primary and the alternate screen each have one) as the emulator changes them,
//! and turns the final byte of a push past a limit no real program reaches into CAN, which makes
//! the emulator abandon that sequence.
//!
//! It also follows the scroll region's top margin on the primary screen, for the command marks:
//! a region that starts below the first line scrolls without filling the history.
//!
//! And it cuts an OSC string longer than [`MAX_OSC`] with CAN: the parser keeps the whole of one
//! in memory until it ends, and `printf '\e]'; cat big.log` never ends it.
//!
//! Only escape sequences matter, and ESC puts the parser in the same state whatever it was
//! reading: so the parser is given each sequence from its ESC, and plain text, along with
//! common sequences that can change none of this (colours, cursor moves), is skipped.

use std::borrow::Cow;

use alacritty_terminal::vte::{Params, Parser, Perform};

/// Pushes beyond this depth are dropped (well below the emulator's 4096).
const MAX_DEPTH: u32 = 1024;
/// Longest OSC string the emulator is given: room for the largest copy kraal takes (OSC 52,
/// in base64: 4/3 of it).
const MAX_OSC: usize = crate::clipboard::MAX_BYTES * 3 / 2;
/// Cancel: ends the sequence being read without acting on it, in any state.
const CAN: u8 = 0x18;
/// Substitute: ends it too.
const SUB: u8 = 0x1a;
const ESC: u8 = 0x1b;

#[derive(Default)]
pub(super) struct KeyboardStackGuard {
    parser: Parser,
    seen: Seen,
    /// Depth of the primary and alternate screens' stacks.
    depth: [u32; 2],
    alt: bool,
    /// The scroll region starts below the first line.
    margin: bool,
    /// A top margin was in force on the primary screen since [`KeyboardStackGuard::take_top_margin`].
    margin_seen: bool,
    /// The parser was given the end of the last read, a sequence it may still be reading: the
    /// next read goes to it up to its first ESC.
    unfinished: bool,
    /// Where the parser is as to OSC strings, after what it was given.
    osc: Osc,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Osc {
    #[default]
    Out,
    /// After an ESC, which a `]` makes an OSC string.
    Esc,
    /// In an OSC string, this long so far.
    In(usize),
}

/// What the bytes from an ESC are, for the guard.
#[derive(Debug, PartialEq)]
enum Sequence {
    /// Bytes to pass over: a sequence that changes nothing followed, up to where the parser is
    /// back in the ground state or at the ESC that ends it.
    Skip(usize),
    /// A control sequence of this length, ending in the ground state, that may change something.
    Read(usize),
    /// Anything else (strings, controls inside a sequence, a sequence the read cut): the parser
    /// reads it, up to the next ESC.
    Other,
}

/// Recognise the sequence at the start of `s` (which starts with ESC) as the parser reads it.
/// Only CSI `u` (keyboard pushes and pops), `h` and `l` (the alternate screen), `r` (the scroll
/// region) and ESC `c` (reset) concern the guard; a CSI sequence of parameter and intermediate
/// bytes and another final byte, an ESC sequence, an OSC, SOS, PM or APC string change nothing
/// it follows. DCS strings, controls inside a sequence and sequences cut by the end of the read
/// go to the parser.
fn sequence(s: &[u8]) -> Sequence {
    match s.get(1) {
        Some(b'[') => {
            let body = s[2..].iter().position(|b| !(0x20..=0x3f).contains(b));
            match body.map(|n| (n + 2, s[n + 2])) {
                Some((at, b'u' | b'h' | b'l' | b'r')) => Sequence::Read(at + 1),
                Some((at, 0x40..=0x7e)) => Sequence::Skip(at + 1),
                _ => Sequence::Other,
            }
        }
        Some(0x20..=0x2f) => match s[1..].iter().position(|b| !(0x20..=0x2f).contains(b)) {
            Some(n) if (0x30..=0x7e).contains(&s[n + 1]) => Sequence::Skip(n + 2),
            _ => Sequence::Other,
        },
        // An OSC string ends with BEL, CAN or SUB, or at the ESC of its terminator.
        Some(b']') => {
            let body = &s[2..];
            let end = memchr::memchr3(0x07, CAN, ESC, body);
            match memchr::memchr(SUB, &body[..end.unwrap_or(body.len())]).or(end) {
                Some(n) if body[n] == ESC => Sequence::Skip(n + 2),
                Some(n) => Sequence::Skip(n + 3),
                None => Sequence::Other,
            }
        }
        // SOS, PM and APC strings end with CAN or SUB, or at an ESC.
        Some(b'X' | b'^' | b'_') => match memchr::memchr3(CAN, SUB, ESC, &s[2..]) {
            Some(n) if s[n + 2] == ESC => Sequence::Skip(n + 2),
            Some(n) => Sequence::Skip(n + 3),
            None => Sequence::Other,
        },
        Some(b'c') => Sequence::Read(2),
        Some(b'P') => Sequence::Other,
        Some(0x30..=0x7e) => Sequence::Skip(2),
        _ => Sequence::Other,
    }
}

/// What the bytes just read do to the stacks, in order.
#[derive(Default)]
struct Seen(Vec<Change>);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    Push,
    Pop(u32),
    /// The alternate screen (1049) set or reset.
    AltScreen(bool),
    /// RIS: both stacks emptied, back on the primary screen.
    Reset,
    /// DECSTBM: whether the scroll region now starts below the first line.
    Margin(bool),
}

impl Perform for Seen {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        // As vte's ANSI performer does: such sequences do nothing.
        if ignore || intermediates.len() > 2 {
            return;
        }
        let first = || params.iter().next().map(|p| p[0]).filter(|p| *p != 0);
        match (action, intermediates) {
            ('u', [b'>']) => self.0.push(Change::Push),
            ('u', [b'<']) => self.0.push(Change::Pop(u32::from(first().unwrap_or(1)))),
            ('h' | 'l', [b'?']) => {
                for _ in params.iter().filter(|p| p[0] == 1049) {
                    self.0.push(Change::AltScreen(action == 'h'));
                }
            }
            ('r', []) => {
                // As the emulator reads it: 0 or nothing is the first line, the bottom must be
                // below the top.
                let mut it = params.iter().map(|p| p[0]);
                let top = it.next().filter(|t| *t != 0).unwrap_or(1);
                let bottom = it.next().filter(|b| *b != 0);
                if bottom.is_none_or(|b| top < b) {
                    self.0.push(Change::Margin(top > 1));
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if byte == b'c' && intermediates.is_empty() {
            self.0.push(Change::Reset);
        }
    }
}

impl KeyboardStackGuard {
    /// `bytes` as the emulator should get them: the same, but for the final byte of a push the
    /// stack has no room for.
    pub(super) fn filter<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        self.note_margin();
        let mut refused = Vec::new();
        let mut at = 0;
        if std::mem::take(&mut self.unfinished) {
            at = memchr::memchr(ESC, bytes).unwrap_or(bytes.len());
            self.read(&bytes[..at], 0, &mut refused);
            self.unfinished = at == bytes.len();
        }
        while let Some(n) = memchr::memchr(ESC, &bytes[at..]) {
            let esc = at + n;
            match sequence(&bytes[esc..]) {
                Sequence::Skip(len) => at = esc + len,
                Sequence::Read(len) => {
                    self.read(&bytes[esc..esc + len], esc, &mut refused);
                    at = esc + len;
                }
                Sequence::Other => {
                    let end = memchr::memchr(ESC, &bytes[esc + 1..]).map_or(bytes.len(), |n| esc + 1 + n);
                    self.read(&bytes[esc..end], esc, &mut refused);
                    self.unfinished = end == bytes.len();
                    at = end;
                }
            }
        }
        if refused.is_empty() {
            return Cow::Borrowed(bytes);
        }
        let mut out = bytes.to_vec();
        for i in refused {
            out[i] = CAN;
        }
        Cow::Owned(out)
    }

    /// Give the parser `bytes` (found at `offset` in the read), noting where the pushes it
    /// refuses end. A push is dispatched on its final `u`: the bytes are read in runs, and each
    /// `u` alone, to know whether it is the one.
    fn read(&mut self, bytes: &[u8], offset: usize, refused: &mut Vec<usize>) {
        let mut bytes = Cow::Borrowed(bytes);
        self.cut_osc(&mut bytes, offset, refused);
        let mut start = 0;
        for at in memchr::memchr_iter(b'u', &bytes) {
            self.read_run(&bytes[start..at]);
            self.parser.advance(&mut self.seen, b"u");
            if self.apply() {
                refused.push(offset + at);
            }
            start = at + 1;
        }
        self.read_run(&bytes[start..]);
    }

    /// Follow `bytes` into and through OSC strings as the parser reads them, and turn the byte
    /// past [`MAX_OSC`] of one into CAN, which ends it. The emulator acts on what it got: a copy
    /// (OSC 52) cut so is longer than any kraal takes.
    fn cut_osc(&mut self, bytes: &mut Cow<[u8]>, offset: usize, refused: &mut Vec<usize>) {
        let mut at = 0;
        while at < bytes.len() {
            match self.osc {
                Osc::Out => match memchr::memchr(ESC, &bytes[at..]) {
                    Some(n) => (at, self.osc) = (at + n + 1, Osc::Esc),
                    None => return,
                },
                Osc::Esc => {
                    // Other controls, and bytes ESC ignores, leave it waiting.
                    if !matches!(bytes[at], 0..=0x17 | 0x19 | 0x1b..=0x1f | 0x7f..=0xff) {
                        self.osc = if bytes[at] == b']' { Osc::In(0) } else { Osc::Out };
                    }
                    at += 1;
                }
                Osc::In(len) => {
                    let rest = &bytes[at..];
                    let body = rest.iter().position(|b| matches!(*b, 0x07 | CAN | SUB | ESC)).unwrap_or(rest.len());
                    if len + body > MAX_OSC {
                        at += MAX_OSC - len;
                        bytes.to_mut()[at] = CAN;
                        refused.push(offset + at);
                        self.osc = Osc::Out;
                    } else if body < rest.len() {
                        // Its end, read again as outside: an ESC starts the next sequence.
                        (at, self.osc) = (at + body, Osc::Out);
                    } else {
                        self.osc = Osc::In(len + body);
                        return;
                    }
                }
            }
        }
    }

    fn read_run(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.parser.advance(&mut self.seen, bytes);
            self.apply();
        }
    }

    /// Follow what was just read, as the emulator will; true when it was a push past the limit
    /// (not counted: the emulator will not get it).
    fn apply(&mut self) -> bool {
        let mut refused = false;
        for change in std::mem::take(&mut self.seen.0) {
            let screen = usize::from(self.alt);
            match change {
                Change::Push if self.depth[screen] >= MAX_DEPTH => refused = true,
                Change::Push => self.depth[screen] += 1,
                Change::Pop(n) => self.depth[screen] = self.depth[screen].saturating_sub(n),
                // The emulator swaps the stacks only when the screen really changes.
                Change::AltScreen(on) => self.alt = on,
                Change::Reset => {
                    self.depth = [0, 0];
                    self.alt = false;
                    self.margin = false;
                }
                Change::Margin(on) => self.margin = on,
            }
            self.note_margin();
        }
        refused
    }

    fn note_margin(&mut self) {
        if self.margin && !self.alt {
            self.margin_seen = true;
        }
    }

    /// Whether a scroll region starting below the first line was in force on the primary screen
    /// since the last call (or still is).
    pub(super) fn take_top_margin(&mut self) -> bool {
        std::mem::take(&mut self.margin_seen) || (self.margin && !self.alt)
    }

    /// The emulator resized: its scroll region is the whole screen again.
    pub(super) fn screen_resized(&mut self) {
        self.margin = false;
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;

    #[test]
    fn pushes_past_the_limit_are_cancelled_and_the_rest_passes() {
        let mut g = KeyboardStackGuard::default();
        let push = b"\x1b[>1u".repeat(MAX_DEPTH as usize + 5);
        let out = g.filter(&push);
        assert_eq!(out.windows(5).filter(|w| *w == b"\x1b[>1u").count(), MAX_DEPTH as usize);
        assert_eq!(out.len(), push.len(), "nothing is removed, the last byte of a push changes");
        // Popping makes room again; other sequences and text are untouched, even when cut.
        let mut out = g.filter(b"\x1b[<2u\x1b[>1u\x1b[>1u\x1b[>1").into_owned();
        out.extend_from_slice(&g.filter(b"u hello \x1b[1;31mred\x1b[0m"));
        assert_eq!(out, b"\x1b[<2u\x1b[>1u\x1b[>1u\x1b[>1\x18 hello \x1b[1;31mred\x1b[0m");
        // The alternate screen has its own stack; RIS empties both.
        let out = g.filter(b"\x1b[?1049h\x1b[>1u\x1b[?1049l\x1b[>1u");
        assert_eq!(&*out, b"\x1b[?1049h\x1b[>1u\x1b[?1049l\x1b[>1\x18");
        let out = g.filter(b"\x1bc\x1b[>1u");
        assert_eq!(&*out, b"\x1bc\x1b[>1u");
        // Output with nothing to change is not copied.
        assert!(matches!(g.filter(b"plain \x1b[1mtext\x1b]0;title\x07\x1b[>1u"), Cow::Borrowed(_)));
    }

    #[test]
    fn sequences_are_recognised_as_the_parser_reads_them() {
        use Sequence::*;
        for (bytes, want) in [
            (&b"\x1b[1;31mtext"[..], Skip(7)),
            (b"\x1b[?25l", Read(6)),
            (b"\x1b[>1u", Read(5)),
            (b"\x1b[2;20r", Read(7)),
            (b"\x1b[1;3", Other),
            (b"\x1b[1\n;3m", Other),
            (b"\x1b(B", Skip(3)),
            (b"\x1b7", Skip(2)),
            (b"\x1bc", Read(2)),
            (b"\x1b]0;title\x07text", Skip(10)),
            (b"\x1b]0;title\x1b\\", Skip(9)),
            (b"\x1b]0;ti\x1atle\x07", Skip(7)),
            (b"\x1b]0;title", Other),
            (b"\x1b_Gkitty\x18", Skip(9)),
            (b"\x1bP1$q\x1b\\", Other),
            (b"\x1b", Other),
            (b"\x1b\x1b[>1u", Other),
        ] {
            assert_eq!(sequence(bytes), want, "{bytes:?}");
        }
    }

    /// An OSC string that never ends (`printf '\e]'; cat big.log`) is cut once it passes MAX_OSC,
    /// across any number of reads: the emulator keeps no more of it, and shows what follows.
    #[test]
    fn an_endless_osc_string_is_cut() {
        let size = super::super::pty::Size { cols: 20, rows: 5 };
        let mut term = Term::new(Config::default(), &size, super::super::pty::Listener::default());
        let mut parser: Processor = Processor::new();
        let mut g = KeyboardStackGuard::default();
        let text = vec![b'x'; 64 * 1024];
        // ESC, then a control the parser passes over, then `]`, each in its own read.
        let reads =
            [&b"\x1b"[..], b"\0", b"]"].into_iter().chain(std::iter::repeat_n(&text[..], MAX_OSC / text.len() + 2));
        let mut out = Vec::new();
        for read in reads {
            let read = g.filter(read);
            parser.advance(&mut term, &read);
            out.extend_from_slice(&read);
        }
        assert_eq!(memchr::memchr_iter(CAN, &out).collect::<Vec<_>>(), [3 + MAX_OSC]);
        parser.advance(&mut term, &g.filter(b"\r\nshown"));
        assert!(super::super::screen::lines_text(&term, 4, 5).starts_with("shown"));
    }

    /// The guard as it was: the parser reads every byte.
    fn filter_every_byte(g: &mut KeyboardStackGuard, bytes: &[u8]) -> Vec<u8> {
        g.note_margin();
        let mut refused = Vec::new();
        g.read(bytes, 0, &mut refused);
        let mut out = bytes.to_vec();
        for i in refused {
            out[i] = CAN;
        }
        out
    }

    /// Skipping text and sequences changes nothing: on random output cut in random reads, the
    /// guard refuses the same pushes and follows the stacks and the margin as one that reads
    /// every byte.
    #[test]
    fn skipping_what_cannot_matter_changes_nothing() {
        const PIECES: &[&[u8]] = &[
            b"\x1b",
            b"[",
            b"]",
            b">",
            b"<",
            b"?",
            b"1",
            b"0",
            b"1049",
            b";",
            b"u",
            b"h",
            b"l",
            b"r",
            b"c",
            b"P",
            b"_",
            b"X",
            b"^",
            b"\\",
            b"m",
            b"$",
            b"(",
            b" ",
            b"text",
            b"\x07",
            b"\x18",
            b"\x1a",
            b"\0",
            b"\x7f",
            b"\x9c",
            b"\xe6\x97\xa5",
            b"\xe6",
            b"\n",
            b"\x1b[>1u",
            b"\x1b[<u",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[3r",
            b"\x1b[r",
            b"\x1bc",
            b"\x1b]0;t\x07",
            b"\x1b[1;31m",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for round in 0..300 {
            let mut fast = KeyboardStackGuard::default();
            let mut slow = KeyboardStackGuard::default();
            // Close to the limit, so that pushes get refused.
            let near = b"\x1b[>1u".repeat(MAX_DEPTH as usize - 3);
            assert_eq!(&*fast.filter(&near), filter_every_byte(&mut slow, &near));
            let mut output = Vec::new();
            for _ in 0..400 {
                output.extend_from_slice(PIECES[next(PIECES.len())]);
            }
            let mut at = 0;
            while at < output.len() {
                let end = (at + 1 + next(40)).min(output.len());
                let read = &output[at..end];
                assert_eq!(&*fast.filter(read), filter_every_byte(&mut slow, read), "round {round}");
                assert_eq!((fast.depth, fast.alt, fast.margin), (slow.depth, slow.alt, slow.margin));
                assert_eq!(fast.take_top_margin(), slow.take_top_margin());
                at = end;
            }
        }
    }

    /// The guard and the emulator read the same sequences: a push the emulator would make is
    /// never missed, whatever surrounds it.
    #[test]
    fn an_emulator_fed_endless_pushes_survives() {
        let size = super::super::pty::Size { cols: 20, rows: 5 };
        let config = Config { kitty_keyboard: true, ..Config::default() };
        for push in [
            &b"\x1b[>1u"[..],
            b"\x1b[\0>1u",
            b"\x1b\0[>1u",
            b"\x1b[>\x071u",
            b"\x1b[\x7f>1u",
            b"\x1b\x7f[>1u",
            b"\x1b\x1b[>1u",
            b"\x1b]0;title\x1b[>1u",
            b"\x1b]0;title\x07\x1b[>1u",
            b"\x1b]0;ti\x18tle\x1b[>1u",
            b"\x1bP1$q\x1b[>1u",
            b"\x1b_kitty\x1b[>1u",
            b"\x1b_kit\x1aty\x1b[>1u",
            b"\xe6\x97\xa5\x1b[>1u",
            b"\x1b[1;31mred\x1b(B\x1b[>1u",
        ] {
            let mut g = KeyboardStackGuard::default();
            let mut term = Term::new(config.clone(), &size, super::super::pty::Listener::default());
            let mut parser: Processor = Processor::new();
            // Cut anywhere: the guard keeps up across reads.
            for chunk in push.repeat(4100).chunks(7) {
                parser.advance(&mut term, &g.filter(chunk));
            }
            assert_eq!(g.depth[0], MAX_DEPTH, "{push:?}");
        }
    }
}
