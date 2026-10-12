//! A filter on pane output before the emulator: keeps the kitty keyboard mode stack bounded.
//!
//! alacritty_terminal 0.26 evicts from the wrong stack when a program pushes more than 4096
//! keyboard modes (`CSI > flags u`): it removes from the title stack, panicking when that one is
//! empty, which would take the whole server down. The guard runs the emulator's own parser
//! (the same vte version, fed the same bytes) one step ahead of it, so it reads every sequence
//! exactly as the emulator will, controls and odd bytes included. It follows the two stacks (the
//! primary and the alternate screen each have one) as the emulator changes them, which a live
//! restart hands over, and turns the final byte of a push past a limit no real program reaches
//! into CAN, which makes the emulator abandon that sequence.
//!
//! It also follows the scroll region's top margin on the primary screen, for the command marks:
//! a region that starts below the first line scrolls without filling the history.

use alacritty_terminal::vte::{Params, Parser, Perform};

/// Pushes beyond this depth are dropped (well below the emulator's 4096).
const MAX_DEPTH: usize = 1024;
/// Cancel: ends the sequence being read without acting on it, in any state.
const CAN: u8 = 0x18;

#[derive(Default)]
pub(super) struct KeyboardStackGuard {
    parser: Parser,
    seen: Seen,
    /// The primary and alternate screens' stacks, as the emulator has them.
    stacks: [Vec<u8>; 2],
    alt: bool,
    /// The scroll region starts below the first line.
    margin: bool,
    /// A top margin was in force on the primary screen since [`KeyboardStackGuard::take_top_margin`].
    margin_seen: bool,
}

/// What the bytes just read do to the stacks, in order.
#[derive(Default)]
struct Seen(Vec<Change>);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    /// The kitty keyboard flags pushed.
    Push(u8),
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
            // As the emulator reads them: the flags it knows of the low byte.
            ('u', [b'>']) => self.0.push(Change::Push(first().unwrap_or(0) as u8 & 0x1f)),
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
    pub(super) fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.note_margin();
        let mut out = Vec::with_capacity(bytes.len());
        // A push is dispatched on its final `u`: everything else is read in runs, and each `u`
        // alone, to know whether it is the one.
        let mut start = 0;
        for at in bytes.iter().enumerate().filter(|(_, b)| **b == b'u').map(|(i, _)| i) {
            self.read(&bytes[start..at]);
            out.extend_from_slice(&bytes[start..at]);
            self.parser.advance(&mut self.seen, b"u");
            let refused = self.apply();
            out.push(if refused { CAN } else { b'u' });
            start = at + 1;
        }
        self.read(&bytes[start..]);
        out.extend_from_slice(&bytes[start..]);
        out
    }

    fn read(&mut self, bytes: &[u8]) {
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
            let stack = &mut self.stacks[screen];
            match change {
                Change::Push(_) if stack.len() >= MAX_DEPTH => refused = true,
                Change::Push(flags) => stack.push(flags),
                Change::Pop(n) => stack.truncate(stack.len().saturating_sub(n as usize)),
                // The emulator swaps the stacks only when the screen really changes.
                Change::AltScreen(on) => self.alt = on,
                Change::Reset => {
                    self.stacks = Default::default();
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

    /// The keyboard flags pushed on the primary and the alternate screen, oldest first.
    pub(super) fn keyboard_stacks(&self) -> &[Vec<u8>; 2] {
        &self.stacks
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
        let push = b"\x1b[>1u".repeat(MAX_DEPTH + 5);
        let out = g.filter(&push);
        assert_eq!(out.windows(5).filter(|w| *w == b"\x1b[>1u").count(), MAX_DEPTH);
        assert_eq!(out.len(), push.len(), "nothing is removed, the last byte of a push changes");
        // Popping makes room again; other sequences and text are untouched, even when cut.
        let mut out = g.filter(b"\x1b[<2u\x1b[>1u\x1b[>1u\x1b[>1");
        out.extend(g.filter(b"u hello \x1b[1;31mred\x1b[0m"));
        assert_eq!(out, b"\x1b[<2u\x1b[>1u\x1b[>1u\x1b[>1\x18 hello \x1b[1;31mred\x1b[0m");
        // The alternate screen has its own stack; RIS empties both.
        let out = g.filter(b"\x1b[?1049h\x1b[>1u\x1b[?1049l\x1b[>1u");
        assert_eq!(out, b"\x1b[?1049h\x1b[>1u\x1b[?1049l\x1b[>1\x18");
        let out = g.filter(b"\x1bc\x1b[>1u");
        assert_eq!(out, b"\x1bc\x1b[>1u");
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
            b"\x1bP1$q\x1b[>1u",
            b"\x1b_kitty\x1b[>1u",
            b"\xe6\x97\xa5\x1b[>1u",
        ] {
            let mut g = KeyboardStackGuard::default();
            let mut term = Term::new(config.clone(), &size, super::super::pty::Listener::default());
            let mut parser: Processor = Processor::new();
            // Cut anywhere: the guard keeps up across reads.
            for chunk in push.repeat(4100).chunks(7) {
                parser.advance(&mut term, &g.filter(chunk));
            }
            assert_eq!(g.stacks[0].len(), MAX_DEPTH, "{push:?}");
        }
    }
}
