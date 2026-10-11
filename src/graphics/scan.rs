//! Picks image sequences out of a pane's output: kitty graphics commands (`ESC _ G … ESC \`)
//! and sixel images (`ESC P … q … ESC \`). Everything else passes through untouched, in order,
//! for the emulator; a sequence cut across reads is held until it is complete.

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// Longest kitty command kept (control data and base64 payload). A transmission that is not
/// chunked can be large; beyond this, the command is dropped.
pub const MAX_APC: usize = 48 << 20;
/// Longest sixel image kept, in encoded bytes.
pub const MAX_SIXEL: usize = 48 << 20;
/// Longest DCS parameter string looked at before passing the sequence on.
const MAX_DCS_PARAMS: usize = 64;

#[derive(Debug, PartialEq)]
pub enum Piece {
    /// Output for the emulator.
    Text(Vec<u8>),
    /// A kitty graphics command: what follows `ESC _ G`, up to the terminator.
    Kitty(Vec<u8>),
    /// A sixel image: the DCS parameters, then the sixel data.
    Sixel { params: Vec<u8>, data: Vec<u8> },
}

#[derive(Default, Clone, Copy, PartialEq, Debug)]
enum State {
    #[default]
    Ground,
    /// After ESC.
    Esc,
    /// After `ESC _`.
    Apc,
    /// Inside a kitty command.
    Kitty,
    /// ESC inside a kitty command: `\` ends it.
    KittyEsc,
    /// After `ESC P`: parameters until the final byte.
    Dcs,
    Sixel,
    SixelEsc,
}

#[derive(Default)]
pub struct Scanner {
    state: State,
    /// Bytes of the sequence being read: its introducer and parameters (passed on as text when
    /// it turns out not to be an image), or its body.
    buf: Vec<u8>,
    params: Vec<u8>,
    /// The body outgrew its limit: it is read to its end and dropped.
    overflow: bool,
}

impl Scanner {
    /// Split `bytes`, continuing what earlier calls left unfinished.
    pub fn split(&mut self, bytes: &[u8]) -> Vec<Piece> {
        let mut out = Vec::new();
        let mut text = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            // Plain text runs to the next ESC in one go.
            if self.state == State::Ground {
                match bytes[i..].iter().position(|b| *b == ESC) {
                    Some(n) => {
                        text.extend_from_slice(&bytes[i..i + n]);
                        i += n;
                    }
                    None => {
                        text.extend_from_slice(&bytes[i..]);
                        break;
                    }
                }
            }
            let b = bytes[i];
            i += 1;
            match self.state {
                State::Ground => {
                    self.buf.clear();
                    self.buf.push(b);
                    self.state = State::Esc;
                }
                State::Esc => self.after_esc(b, &mut text),
                State::Apc if b == b'G' => {
                    self.buf.clear();
                    self.overflow = false;
                    self.state = State::Kitty;
                }
                State::Apc => {
                    // Another kind of APC: the emulator has its say.
                    text.extend_from_slice(&self.buf);
                    self.buf.clear();
                    self.reprocess(b, &mut text);
                }
                State::Kitty | State::Sixel => {
                    if b == ESC {
                        self.state = if self.state == State::Kitty { State::KittyEsc } else { State::SixelEsc };
                    } else if b == CAN || b == SUB {
                        self.abort();
                    } else {
                        let limit = if self.state == State::Kitty { MAX_APC } else { MAX_SIXEL };
                        if self.buf.len() < limit {
                            self.buf.push(b);
                        } else {
                            self.overflow = true;
                        }
                    }
                }
                State::KittyEsc | State::SixelEsc => {
                    if b == b'\\' {
                        if !self.overflow {
                            let body = std::mem::take(&mut self.buf);
                            if self.state == State::KittyEsc {
                                flush(&mut text, &mut out);
                                out.push(Piece::Kitty(body));
                            } else {
                                flush(&mut text, &mut out);
                                out.push(Piece::Sixel { params: std::mem::take(&mut self.params), data: body });
                            }
                        }
                        self.abort();
                    } else {
                        // ESC and something else cancels the string and starts a new sequence.
                        self.abort();
                        self.buf.push(ESC);
                        self.state = State::Esc;
                        self.after_esc(b, &mut text);
                    }
                }
                State::Dcs => match b {
                    0x30..=0x3f if self.params.len() < MAX_DCS_PARAMS => {
                        self.params.push(b);
                        self.buf.push(b);
                    }
                    b'q' => {
                        self.buf.clear();
                        self.overflow = false;
                        self.state = State::Sixel;
                    }
                    _ => {
                        // Not a sixel image (DECRQSS, tmux passthrough…): passed on as it came.
                        text.extend_from_slice(&self.buf);
                        self.buf.clear();
                        self.params.clear();
                        self.reprocess(b, &mut text);
                    }
                },
            }
        }
        flush(&mut text, &mut out);
        out
    }

    fn after_esc(&mut self, b: u8, text: &mut Vec<u8>) {
        match b {
            b'_' => {
                self.buf.push(b);
                self.state = State::Apc;
            }
            b'P' => {
                self.buf.push(b);
                self.params.clear();
                self.state = State::Dcs;
            }
            ESC => {
                // The first ESC led nowhere we care about.
                text.extend_from_slice(&self.buf);
                self.buf.clear();
                self.buf.push(ESC);
            }
            _ => {
                text.extend_from_slice(&self.buf);
                text.push(b);
                self.buf.clear();
                self.state = State::Ground;
            }
        }
    }

    /// `b` ended a sequence that is passed on: read it again from the ground state.
    fn reprocess(&mut self, b: u8, text: &mut Vec<u8>) {
        self.state = State::Ground;
        if b == ESC {
            self.buf.push(ESC);
            self.state = State::Esc;
        } else {
            text.push(b);
        }
    }

    fn abort(&mut self) {
        self.state = State::Ground;
        self.buf.clear();
        self.params.clear();
        self.overflow = false;
    }
}

fn flush(text: &mut Vec<u8>, out: &mut Vec<Piece>) {
    if !text.is_empty() {
        out.push(Piece::Text(std::mem::take(text)));
    }
}
