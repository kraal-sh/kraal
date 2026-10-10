//! Terminal input: parsing what the client's terminal sends, and encoding it for panes.
//!
//! The client puts the outer terminal in raw mode with SGR mouse reporting, bracketed paste and
//! focus events enabled, and forwards its stdin bytes verbatim. [`InputParser`] turns that stream
//! into [`InputEvent`]s: legacy xterm keys, SGR mouse reports, pastes, focus changes, and (when
//! tolerated) CSI u / `modifyOtherKeys` keys. Terminal responses to queries (OSC/DCS/APC/PM/SOS
//! strings, CSI reports) come out as [`InputEvent::Raw`]. The `encode_*` functions produce what a
//! pane program expects given the modes it enabled ([`PaneInputModes`]), and [`KeySpec`] parses
//! and matches keybindings from the configuration.
//!
//! Known ambiguities of the legacy encoding: `ESC [ 1 ; m R` is read as a modified F3 even though
//! a cursor position report for row 1 looks the same, and a chunk ending right after `ESC [`,
//! `ESC O`, `ESC ]`, `ESC P`, `ESC _`, `ESC ^` or `ESC X` stays pending until [`InputParser::flush`]
//! turns it into the corresponding Alt+key.

use std::fmt;
use std::str::FromStr;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Longest CSI sequence accepted before giving up and emitting it as raw bytes.
const MAX_CSI_LEN: usize = 256;
/// Longest OSC/DCS/APC/PM/SOS string accepted before giving up and emitting it as raw bytes.
const MAX_STRING_LEN: usize = 64 * 1024;
/// `CSI n ~` numbers of F5..=F20.
const F_TILDE: [u32; 16] = [15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34];

/// Modifier keys held with a key or mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    /// No modifier.
    pub const NONE: Mods = Mods { shift: false, alt: false, ctrl: false };
    /// Shift alone.
    #[cfg(test)]
    pub const SHIFT: Mods = Mods { shift: true, alt: false, ctrl: false };
    /// Alt alone.
    pub const ALT: Mods = Mods { shift: false, alt: true, ctrl: false };
    /// Ctrl alone.
    pub const CTRL: Mods = Mods { shift: false, alt: false, ctrl: true };

    /// True when no modifier is held.
    pub fn is_empty(self) -> bool {
        !(self.shift || self.alt || self.ctrl)
    }

    /// Decode an xterm modifier parameter (`1 + bits`: shift 1, alt 2, ctrl 4, meta 8).
    /// Meta is folded into alt; higher bits (kitty's hyper, caps/num lock) are ignored.
    pub fn from_xterm_param(param: u32) -> Mods {
        let bits = param.saturating_sub(1);
        Mods { shift: bits & 1 != 0, alt: bits & (2 | 8) != 0, ctrl: bits & 4 != 0 }
    }

    /// The xterm modifier parameter for these modifiers (`1` when none).
    pub fn xterm_param(self) -> u32 {
        1 + u32::from(self.shift) + 2 * u32::from(self.alt) + 4 * u32::from(self.ctrl)
    }
}

/// A key, independent of its encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    /// A character. Received Ctrl+letter is reported lowercase; a plain uppercase ASCII letter
    /// carries the shift modifier.
    Char(char),
    Enter,
    Tab,
    /// Shift+Tab (`ESC [ Z`); always carries the shift modifier when parsed.
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Function key F1..=F20.
    F(u8),
}

/// A key press received from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Mods,
    /// Exact bytes received (empty for keys synthesised by the server).
    pub raw: Vec<u8>,
}

impl Key {
    /// A synthesised key with no received bytes; [`encode_key`] encodes it from scratch.
    #[cfg(test)]
    pub fn new(code: KeyCode, mods: Mods) -> Key {
        Key { code, mods, raw: Vec::new() }
    }
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// What happened in a mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press(MouseButton),
    Release(MouseButton),
    /// Motion with a button held.
    Drag(MouseButton),
    /// Motion with no button held.
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

/// A mouse event, with coordinates in the outer terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    /// 0-based column.
    pub col: u16,
    /// 0-based row.
    pub row: u16,
    pub mods: Mods,
}

/// An event parsed from the client's input stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key(Key),
    Mouse(MouseEvent),
    /// Content of a bracketed paste, without the markers. Never empty.
    Paste(Vec<u8>),
    FocusGained,
    FocusLost,
    /// Bytes that are not a recognised key/mouse/paste/focus event (terminal responses, unknown
    /// sequences, invalid UTF-8), to forward verbatim or inspect.
    Raw(Vec<u8>),
}

/// Incremental parser for the bytes the client forwards from its terminal.
#[derive(Debug, Default)]
pub struct InputParser {
    /// Bytes not consumed yet (an incomplete sequence, or a possible paste end marker prefix).
    buf: Vec<u8>,
    /// Content of the bracketed paste in progress, if any.
    paste: Option<Vec<u8>>,
    /// Last pressed button, to resolve X10 releases (which do not say which button).
    last_button: Option<MouseButton>,
}

impl InputParser {
    /// An empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a chunk read from the client. Complete events are returned; an incomplete escape
    /// sequence at the end is kept for the next call. Exception: a chunk ending with a lone ESC
    /// (exactly one 0x1b with nothing after it) yields [`KeyCode::Esc`] immediately, since
    /// terminals send escape sequences in one write.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.buf.extend_from_slice(bytes);
        self.run(false)
    }

    /// True if [`flush`](Self::flush) would produce something: an incomplete sequence, or paste
    /// content received while the paste end marker has not arrived yet.
    pub fn has_pending(&self) -> bool {
        match &self.paste {
            Some(content) => !content.is_empty(),
            None => !self.buf.is_empty(),
        }
    }

    /// Called after a short timeout when [`has_pending`](Self::has_pending): turn whatever is
    /// pending into events. `ESC` + one printable byte (e.g. `ESC [`) becomes Alt+that key, other
    /// partial sequences become [`InputEvent::Raw`]. Inside a bracketed paste, the content
    /// received so far is emitted as a `Paste` and the parser keeps waiting for the end marker.
    pub fn flush(&mut self) -> Vec<InputEvent> {
        self.run(true)
    }

    fn run(&mut self, flush: bool) -> Vec<InputEvent> {
        let mut events = Vec::new();
        let mut pos = 0;
        loop {
            if let Some(content) = self.paste.as_mut() {
                let rest = &self.buf[pos..];
                if let Some(end) = find(rest, PASTE_END) {
                    content.extend_from_slice(&rest[..end]);
                    push_paste(&mut events, content);
                    self.paste = None;
                    pos += end + PASTE_END.len();
                    continue;
                }
                // Keep a possible prefix of the end marker for the next chunk.
                let take = rest.len() - partial_suffix(rest, PASTE_END);
                content.extend_from_slice(&rest[..take]);
                pos += take;
                if flush {
                    push_paste(&mut events, content);
                }
                break;
            }
            let rest = &self.buf[pos..];
            if rest.is_empty() {
                break;
            }
            match parse_one(rest) {
                Step::Done(tok, len) => {
                    pos += len;
                    self.accept(tok, &mut events);
                }
                Step::Incomplete => {
                    if flush {
                        events.push(flush_partial(rest));
                        pos = self.buf.len();
                    } else if rest == [ESC] {
                        events.push(InputEvent::Key(Key { code: KeyCode::Esc, mods: Mods::NONE, raw: vec![ESC] }));
                        pos += 1;
                    }
                    break;
                }
            }
        }
        self.buf.drain(..pos);
        events
    }

    fn accept(&mut self, tok: Tok, events: &mut Vec<InputEvent>) {
        match tok {
            Tok::Event(ev) => {
                if let InputEvent::Mouse(MouseEvent { kind: MouseKind::Press(button), .. }) = &ev {
                    self.last_button = Some(*button);
                }
                events.push(ev);
            }
            Tok::PasteStart => self.paste = Some(Vec::new()),
            Tok::X10Release(mut ev) => {
                let button = self.last_button.take().unwrap_or(MouseButton::Left);
                ev.kind = MouseKind::Release(button);
                events.push(InputEvent::Mouse(ev));
            }
        }
    }
}

/// What a parse step produced.
enum Tok {
    Event(InputEvent),
    /// `ESC [ 200 ~`: start of a bracketed paste.
    PasteStart,
    /// An X10 mouse release, which does not say which button was released.
    X10Release(MouseEvent),
}

enum Step {
    /// A token and the number of bytes it consumed.
    Done(Tok, usize),
    /// The bytes are a prefix of a longer sequence.
    Incomplete,
}

fn push_paste(events: &mut Vec<InputEvent>, content: &mut Vec<u8>) {
    if !content.is_empty() {
        events.push(InputEvent::Paste(std::mem::take(content)));
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Length of the longest proper prefix of `marker` that `data` ends with.
fn partial_suffix(data: &[u8], marker: &[u8]) -> usize {
    (1..marker.len()).rev().find(|&n| data.ends_with(&marker[..n])).unwrap_or(0)
}

/// Events for an incomplete sequence when the caller gives up waiting for the rest.
fn flush_partial(rest: &[u8]) -> InputEvent {
    let (code, mods) = match *rest {
        [ESC] => (KeyCode::Esc, Mods::NONE),
        [ESC, ESC] => (KeyCode::Esc, Mods::ALT),
        [ESC, c @ 0x20..=0x7e] => (KeyCode::Char(char::from(c)), Mods::ALT),
        _ => return InputEvent::Raw(rest.to_vec()),
    };
    InputEvent::Key(make_key(code, mods, rest))
}

/// Apply the normalisation rules for received keys.
fn normalize(code: KeyCode, mut mods: Mods) -> (KeyCode, Mods) {
    match code {
        KeyCode::Char(c) if c.is_ascii_alphabetic() => {
            if c.is_ascii_uppercase() {
                mods.shift = true;
            }
            let c = if mods.ctrl {
                c.to_ascii_lowercase()
            } else if mods.shift {
                c.to_ascii_uppercase()
            } else {
                c
            };
            (KeyCode::Char(c), mods)
        }
        KeyCode::BackTab => (code, Mods { shift: true, ..mods }),
        _ => (code, mods),
    }
}

fn make_key(code: KeyCode, mods: Mods, raw: &[u8]) -> Key {
    let (code, mods) = normalize(code, mods);
    Key { code, mods, raw: raw.to_vec() }
}

fn key_tok(code: KeyCode, mods: Mods, raw: &[u8]) -> Tok {
    Tok::Event(InputEvent::Key(make_key(code, mods, raw)))
}

fn raw_tok(raw: &[u8]) -> Tok {
    Tok::Event(InputEvent::Raw(raw.to_vec()))
}

fn with_alt(mods: Mods) -> Mods {
    Mods { alt: true, ..mods }
}

/// Parse one event at the start of `b` (non-empty).
fn parse_one(b: &[u8]) -> Step {
    if b[0] == ESC { parse_escape(b) } else { parse_plain(b) }
}

/// A byte that is not ESC: control character, ASCII or UTF-8.
fn parse_plain(b: &[u8]) -> Step {
    let byte = b[0];
    let one = |code, mods| Step::Done(key_tok(code, mods, &b[..1]), 1);
    match byte {
        0x00 => one(KeyCode::Char(' '), Mods::CTRL),
        b'\t' => one(KeyCode::Tab, Mods::NONE),
        b'\r' => one(KeyCode::Enter, Mods::NONE),
        0x01..=0x1a => one(KeyCode::Char(char::from(b'a' + byte - 1)), Mods::CTRL),
        ESC => one(KeyCode::Esc, Mods::NONE),
        0x1c..=0x1f => {
            let c = ['\\', ']', '^', '_'][usize::from(byte - 0x1c)];
            one(KeyCode::Char(c), Mods::CTRL)
        }
        0x20..=0x7e => one(KeyCode::Char(char::from(byte)), Mods::NONE),
        0x7f => one(KeyCode::Backspace, Mods::NONE),
        _ => parse_utf8(b),
    }
}

/// A multi-byte UTF-8 character, or a single invalid byte as raw.
fn parse_utf8(b: &[u8]) -> Step {
    let invalid = || Step::Done(raw_tok(&b[..1]), 1);
    let need = match b[0] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return invalid(),
    };
    let avail = b.len().min(need);
    if b[1..avail].iter().any(|&c| c & 0xc0 != 0x80) {
        return invalid();
    }
    if b.len() < need {
        return Step::Incomplete;
    }
    match std::str::from_utf8(&b[..need]).ok().and_then(|s| s.chars().next()) {
        Some(c) => Step::Done(key_tok(KeyCode::Char(c), Mods::NONE, &b[..need]), need),
        None => invalid(),
    }
}

/// A sequence starting with ESC.
fn parse_escape(b: &[u8]) -> Step {
    let Some(&next) = b.get(1) else {
        return Step::Incomplete;
    };
    match next {
        b'[' => parse_csi(b),
        b'O' => parse_ss3(b),
        b']' | b'P' | b'_' | b'^' | b'X' => parse_string(b),
        ESC => parse_double_escape(b),
        // Alt + a plain key.
        _ => match parse_plain(&b[1..]) {
            Step::Incomplete => Step::Incomplete,
            Step::Done(Tok::Event(InputEvent::Key(k)), n) => {
                Step::Done(key_tok(k.code, with_alt(k.mods), &b[..=n]), n + 1)
            }
            Step::Done(..) => Step::Done(key_tok(KeyCode::Esc, Mods::NONE, &b[..1]), 1),
        },
    }
}

/// `ESC ESC ...`: Alt+Esc, or an ESC-prefixed (Alt) CSI/SS3 key as some terminals send.
fn parse_double_escape(b: &[u8]) -> Step {
    match b.get(2) {
        None => Step::Incomplete,
        Some(b'[' | b'O') => match parse_escape(&b[1..]) {
            Step::Incomplete => Step::Incomplete,
            // n == 2 is the Alt+[ / Alt+O fallback, not a real sequence.
            Step::Done(Tok::Event(InputEvent::Key(k)), n) if n > 2 => {
                Step::Done(key_tok(k.code, with_alt(k.mods), &b[..=n]), n + 1)
            }
            Step::Done(..) => Step::Done(key_tok(KeyCode::Esc, Mods::NONE, &b[..1]), 1),
        },
        Some(_) => Step::Done(key_tok(KeyCode::Esc, Mods::ALT, &b[..2]), 2),
    }
}

/// Final byte of an SS3 sequence (`ESC O x`), including application keypad keys.
fn ss3_code(c: u8) -> Option<KeyCode> {
    Some(match c {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P'..=b'S' => KeyCode::F(c - b'P' + 1),
        b'M' => KeyCode::Enter,
        b'X' => KeyCode::Char('='),
        b'j' => KeyCode::Char('*'),
        b'k' => KeyCode::Char('+'),
        b'l' => KeyCode::Char(','),
        b'm' => KeyCode::Char('-'),
        b'n' => KeyCode::Char('.'),
        b'o' => KeyCode::Char('/'),
        b'p'..=b'y' => KeyCode::Char(char::from(c - b'p' + b'0')),
        _ => return None,
    })
}

/// `ESC O x`, optionally `ESC O <modifier digit> x` (old xterm). Unknown: Alt+O.
fn parse_ss3(b: &[u8]) -> Step {
    let Some(&c) = b.get(2) else {
        return Step::Incomplete;
    };
    let (mods, i) = if c.is_ascii_digit() {
        if b.len() < 4 {
            return Step::Incomplete;
        }
        (Mods::from_xterm_param(u32::from(c - b'0')), 3)
    } else {
        (Mods::NONE, 2)
    };
    match ss3_code(b[i]) {
        Some(code) => Step::Done(key_tok(code, mods, &b[..=i]), i + 1),
        None => Step::Done(key_tok(KeyCode::Char('O'), Mods::ALT, &b[..2]), 2),
    }
}

/// `ESC [ ...`: CSI sequence, or X10 mouse report (`ESC [ M Cb Cx Cy`).
fn parse_csi(b: &[u8]) -> Step {
    if b.get(2) == Some(&b'M') {
        return if b.len() < 6 { Step::Incomplete } else { Step::Done(x10_mouse(&b[..6]), 6) };
    }
    let mut i = 2;
    while i < b.len() && i < MAX_CSI_LEN {
        match b[i] {
            0x20..=0x3f => i += 1,
            0x40..=0x7e => return Step::Done(csi_dispatch(&b[..=i]), i + 1),
            // Not a CSI after all: `ESC [` alone is Alt+[.
            _ if i == 2 => return Step::Done(key_tok(KeyCode::Char('['), Mods::ALT, &b[..2]), 2),
            // Interrupted (control byte, new ESC, non-ASCII): keep what we have as raw.
            _ => return Step::Done(raw_tok(&b[..i]), i),
        }
    }
    if i >= MAX_CSI_LEN { Step::Done(raw_tok(&b[..i]), i) } else { Step::Incomplete }
}

/// A string sequence (OSC, DCS, APC, PM, SOS) terminated by BEL or ST, as raw bytes.
fn parse_string(b: &[u8]) -> Step {
    if b.len() == 2 {
        return Step::Incomplete;
    }
    for i in 2..b.len().min(MAX_STRING_LEN) {
        match b[i] {
            BEL => return Step::Done(raw_tok(&b[..=i]), i + 1),
            ESC => {
                return match b.get(i + 1) {
                    None => Step::Incomplete,
                    Some(b'\\') => Step::Done(raw_tok(&b[..i + 2]), i + 2),
                    // Aborted by a new escape sequence.
                    Some(_) => Step::Done(raw_tok(&b[..i]), i),
                };
            }
            _ => {}
        }
    }
    if b.len() >= MAX_STRING_LEN { Step::Done(raw_tok(&b[..MAX_STRING_LEN]), MAX_STRING_LEN) } else { Step::Incomplete }
}

/// Parse `;`-separated parameters with `:` sub-parameters (empty = 0). None if malformed.
fn parse_params(body: &[u8]) -> Option<Vec<Vec<u32>>> {
    if body.is_empty() {
        return Some(Vec::new());
    }
    body.split(|&c| c == b';').map(|param| param.split(|&c| c == b':').map(parse_num).collect()).collect()
}

fn parse_num(s: &[u8]) -> Option<u32> {
    s.iter().try_fold(0u32, |acc, &c| {
        c.is_ascii_digit().then(|| acc.saturating_mul(10).saturating_add(u32::from(c - b'0')))
    })
}

/// First sub-parameter of parameter `i` (0 when absent).
fn param(params: &[Vec<u32>], i: usize) -> u32 {
    sub_param(params, i, 0)
}

fn sub_param(params: &[Vec<u32>], i: usize, j: usize) -> u32 {
    params.get(i).and_then(|p| p.get(j)).copied().unwrap_or(0)
}

/// Modifier parameter of a legacy key, None if outside xterm's range (likely a report).
fn legacy_mods(params: &[Vec<u32>], i: usize) -> Option<Mods> {
    let m = param(params, i);
    (m <= 16).then(|| Mods::from_xterm_param(m))
}

/// A complete CSI sequence `seq` (`ESC [ ... final`).
fn csi_dispatch(seq: &[u8]) -> Tok {
    let final_byte = seq[seq.len() - 1];
    let body = &seq[2..seq.len() - 1];
    let (marker, body) = match body.first() {
        Some(&m @ (b'<' | b'=' | b'>' | b'?')) => (Some(m), &body[1..]),
        _ => (None, body),
    };
    let Some(params) = parse_params(body) else {
        return raw_tok(seq);
    };
    match (marker, final_byte) {
        (Some(b'<'), b'M' | b'm') => sgr_mouse(&params, final_byte == b'm', seq),
        (None, _) => csi_key(&params, final_byte, seq),
        _ => raw_tok(seq),
    }
}

fn csi_key(params: &[Vec<u32>], final_byte: u8, seq: &[u8]) -> Tok {
    let code = match final_byte {
        b'I' if params.is_empty() => return Tok::Event(InputEvent::FocusGained),
        b'O' if params.is_empty() => return Tok::Event(InputEvent::FocusLost),
        b'~' => return csi_tilde(params, seq),
        b'u' => return csi_u(params, seq),
        b'A'..=b'D' | b'H' | b'F' | b'P'..=b'S' => ss3_code(final_byte),
        b'Z' => Some(KeyCode::BackTab),
        _ => None,
    };
    // Only `CSI x` and `CSI 1 ; m x`: anything else (e.g. a cursor position report) is raw.
    match (code, legacy_mods(params, 1)) {
        (Some(code), Some(mods)) if params.len() <= 2 && param(params, 0) <= 1 => key_tok(code, mods, seq),
        _ => raw_tok(seq),
    }
}

fn csi_tilde(params: &[Vec<u32>], seq: &[u8]) -> Tok {
    let n = param(params, 0);
    let code = match n {
        200 if params.len() == 1 => return Tok::PasteStart,
        // xterm modifyOtherKeys: CSI 27 ; mods ; codepoint ~
        27 if params.len() == 3 => {
            let mods = Mods::from_xterm_param(param(params, 1));
            return codepoint_key(param(params, 2), 0, mods, seq);
        }
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11..=14 => KeyCode::F((n - 10) as u8),
        _ => match F_TILDE.iter().position(|&v| v == n) {
            Some(i) => KeyCode::F(i as u8 + 5),
            None => return raw_tok(seq),
        },
    };
    match legacy_mods(params, 1) {
        Some(mods) if params.len() <= 2 => key_tok(code, mods, seq),
        _ => raw_tok(seq),
    }
}

/// `CSI codepoint[:shifted[:base]] [; mods[:event] [; text]] u` (kitty / fixterms).
fn csi_u(params: &[Vec<u32>], seq: &[u8]) -> Tok {
    // Release events (event type 3) are not key presses.
    if params.is_empty() || params.len() > 3 || sub_param(params, 1, 1) == 3 {
        return raw_tok(seq);
    }
    let mods = Mods::from_xterm_param(param(params, 1));
    codepoint_key(param(params, 0), sub_param(params, 0, 1), mods, seq)
}

fn codepoint_key(cp: u32, shifted: u32, mods: Mods, seq: &[u8]) -> Tok {
    let code = match cp {
        9 if mods.shift => KeyCode::BackTab,
        9 => KeyCode::Tab,
        13 => KeyCode::Enter,
        27 => KeyCode::Esc,
        8 | 127 => KeyCode::Backspace,
        _ => {
            let cp = if mods.shift && shifted != 0 { shifted } else { cp };
            match char::from_u32(cp) {
                // Private use area: kitty functional keys (keypad, media, lone modifiers).
                Some(c) if !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&c) => KeyCode::Char(c),
                _ => return raw_tok(seq),
            }
        }
    };
    key_tok(code, mods, seq)
}

fn mouse_mods(cb: u32) -> Mods {
    Mods { shift: cb & 4 != 0, alt: cb & 8 != 0, ctrl: cb & 16 != 0 }
}

/// Decode a mouse button byte (without the X10 +32 offset). None for unsupported buttons.
fn decode_mouse(cb: u32, release: bool) -> Option<MouseKind> {
    if cb >= 128 {
        return None; // buttons 8-11
    }
    let button = match cb & 3 {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        _ => None,
    };
    Some(if cb & 64 != 0 {
        match cb & 3 {
            0 => MouseKind::ScrollUp,
            1 => MouseKind::ScrollDown,
            2 => MouseKind::ScrollLeft,
            _ => MouseKind::ScrollRight,
        }
    } else if cb & 32 != 0 {
        button.map_or(MouseKind::Move, MouseKind::Drag)
    } else if release {
        MouseKind::Release(button?)
    } else {
        MouseKind::Press(button?)
    })
}

/// 1-based terminal coordinate to 0-based u16.
fn coord(v: u32) -> u16 {
    u16::try_from(v.saturating_sub(1)).unwrap_or(u16::MAX)
}

/// `ESC [ < b ; x ; y M|m`.
fn sgr_mouse(params: &[Vec<u32>], release: bool, seq: &[u8]) -> Tok {
    let cb = param(params, 0);
    match decode_mouse(cb, release) {
        Some(kind) if params.len() == 3 => Tok::Event(InputEvent::Mouse(MouseEvent {
            kind,
            col: coord(param(params, 1)),
            row: coord(param(params, 2)),
            mods: mouse_mods(cb),
        })),
        _ => raw_tok(seq),
    }
}

/// `ESC [ M Cb Cx Cy` with +32 offsets.
fn x10_mouse(seq: &[u8]) -> Tok {
    if seq[3..6].iter().any(|&v| v < 32) {
        return raw_tok(seq);
    }
    let cb = u32::from(seq[3] - 32);
    let mut ev = MouseEvent {
        kind: MouseKind::Move,
        col: coord(u32::from(seq[4] - 32)),
        row: coord(u32::from(seq[5] - 32)),
        mods: mouse_mods(cb),
    };
    if cb & (32 | 64) == 0 && cb & 3 == 3 {
        return Tok::X10Release(ev);
    }
    match decode_mouse(cb, false) {
        Some(kind) => {
            ev.kind = kind;
            Tok::Event(InputEvent::Mouse(ev))
        }
        None => raw_tok(seq),
    }
}

/// Terminal modes of the destination pane, read from its emulator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneInputModes {
    /// DECCKM (`?1`): cursor keys send SS3.
    pub app_cursor: bool,
    /// DECKPAM (`ESC =`): keypad sends SS3.
    pub app_keypad: bool,
    /// `?2004`.
    pub bracketed_paste: bool,
    /// `?1000`: press/release/wheel.
    pub mouse_click: bool,
    /// `?1002`: also motion with a button held.
    pub mouse_drag: bool,
    /// `?1003`: all motion.
    pub mouse_motion: bool,
    /// `?1006`: SGR mouse encoding.
    pub sgr_mouse: bool,
    /// `?1005`: UTF-8 mouse encoding.
    pub utf8_mouse: bool,
    /// `?1004`.
    pub focus_events: bool,
    /// `?1007`: wheel sends arrow keys on the alternate screen.
    pub alternate_scroll: bool,
    /// The alternate screen is active.
    pub alt_screen: bool,
    /// Kitty keyboard protocol flags the pane enabled (`CSI > flags u`): 1 disambiguate,
    /// 2 event types, 4 alternate keys, 8 all keys as escape codes, 16 associated text.
    pub kitty_flags: u8,
}

/// Bytes to write to the pane PTY for this key. Usually `key.raw`, but arrows/Home/End without
/// modifiers are re-encoded for the pane's cursor mode (SS3 `ESC O A` with `app_cursor`, CSI
/// otherwise), keypad SS3 keys become plain keys when the pane is not in keypad mode, and keys
/// received as CSI u / `modifyOtherKeys` (or synthesised with empty `raw`) get a canonical
/// legacy encoding.
pub fn encode_key(key: &Key, modes: PaneInputModes) -> Vec<u8> {
    use KeyCode::*;
    if modes.kitty_flags != 0
        && let Some(bytes) = encode_kitty(key.code, key.mods, modes.kitty_flags)
    {
        return bytes;
    }
    let cursor_key = matches!(key.code, Up | Down | Left | Right | Home | End) && key.mods.is_empty();
    let keypad = !modes.app_keypad && key.raw.starts_with(b"\x1bO") && matches!(key.code, Char(_) | Enter);
    if key.raw.is_empty() || cursor_key || keypad || is_extended_encoding(&key.raw) {
        encode_key_code(key.code, key.mods, modes)
    } else {
        key.raw.clone()
    }
}

/// Kitty keyboard protocol encoding for a pane that enabled it, for the keys whose legacy
/// encoding is ambiguous or lossy (Shift+Enter, Ctrl+Enter, Alt+letters, Esc…). `None` means
/// the legacy encoding is also what the protocol sends (plain text, arrows, function keys).
pub fn encode_kitty(code: KeyCode, mods: Mods, flags: u8) -> Option<Vec<u8>> {
    use KeyCode::*;
    let all_keys = flags & 8 != 0;
    let bits = u32::from(mods.shift) | (u32::from(mods.alt) << 1) | (u32::from(mods.ctrl) << 2);
    let m = 1 + bits;
    let seq = |cp: u32, m: u32| -> Vec<u8> {
        if m == 1 { format!("\x1b[{cp}u").into_bytes() } else { format!("\x1b[{cp};{m}u").into_bytes() }
    };
    match code {
        Esc => Some(seq(27, m)),
        Enter | Tab | Backspace if all_keys || m != 1 => {
            let cp = match code {
                Enter => 13,
                Tab => 9,
                _ => 127,
            };
            Some(seq(cp, m))
        }
        BackTab if all_keys => Some(seq(9, 1 + (bits | 1))),
        Char(c) if all_keys || mods.ctrl || mods.alt => {
            // The key is reported unshifted, with shift as a modifier.
            let base = if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c };
            let m = if c.is_ascii_uppercase() { 1 + (bits | 1) } else { m };
            Some(seq(base as u32, m))
        }
        _ => None,
    }
}

/// Whether `raw` is a CSI u or `modifyOtherKeys` sequence, possibly ESC-prefixed.
fn is_extended_encoding(raw: &[u8]) -> bool {
    let seq = match raw.strip_prefix(&[ESC]) {
        Some(rest) if rest.starts_with(&[ESC]) => rest,
        _ => raw,
    };
    seq.starts_with(b"\x1b[") && (seq.ends_with(b"u") || (seq.starts_with(b"\x1b[27;") && seq.ends_with(b"~")))
}

/// C0 byte sent for Ctrl+`c` in the legacy encoding, if any.
fn control_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' | 'A'..='Z' => c as u8 & 0x1f,
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' | '~' => 0x1e,
        '_' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Encode a key from scratch in the legacy xterm encoding (used when the server synthesises
/// keys, e.g. alternate-scroll arrows or `send-keys`). Modifiers that the legacy encoding cannot
/// express (e.g. Ctrl+Enter, Ctrl+1) are dropped; F keys outside F1..=F20 encode to nothing.
pub fn encode_key_code(code: KeyCode, mods: Mods, modes: PaneInputModes) -> Vec<u8> {
    let mut out = Vec::new();
    let alt = if mods.alt { &[ESC][..] } else { &[] };
    let csi = |n: u32, final_byte: char| -> Vec<u8> {
        if mods.is_empty() {
            match n {
                1 => format!("\x1b[{final_byte}"),
                _ => format!("\x1b[{n}{final_byte}"),
            }
        } else {
            format!("\x1b[{n};{}{final_byte}", mods.xterm_param())
        }
        .into_bytes()
    };
    match code {
        KeyCode::Char(c) => {
            out.extend_from_slice(alt);
            match control_byte(c).filter(|_| mods.ctrl) {
                Some(b) => out.push(b),
                None => {
                    let c = if mods.shift { c.to_ascii_uppercase() } else { c };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
            }
        }
        KeyCode::Enter => out.extend_from_slice(&[alt, b"\r"].concat()),
        KeyCode::Tab if !mods.shift => out.extend_from_slice(&[alt, b"\t"].concat()),
        KeyCode::Tab | KeyCode::BackTab => {
            if mods.alt || mods.ctrl {
                let m = Mods { shift: true, ..mods };
                out = format!("\x1b[1;{}Z", m.xterm_param()).into_bytes();
            } else {
                out.extend_from_slice(b"\x1b[Z");
            }
        }
        KeyCode::Backspace => {
            out.extend_from_slice(alt);
            out.push(if mods.ctrl { 0x08 } else { 0x7f });
        }
        KeyCode::Esc => out.extend_from_slice(&[alt, &[ESC]].concat()),
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left | KeyCode::Home | KeyCode::End => {
            let final_byte = match code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                _ => 'F',
            };
            out = if mods.is_empty() && modes.app_cursor {
                format!("\x1bO{final_byte}").into_bytes()
            } else {
                csi(1, final_byte)
            };
        }
        KeyCode::Insert => out = csi(2, '~'),
        KeyCode::Delete => out = csi(3, '~'),
        KeyCode::PageUp => out = csi(5, '~'),
        KeyCode::PageDown => out = csi(6, '~'),
        KeyCode::F(n @ 1..=4) => {
            let final_byte = char::from(b'P' + n - 1);
            out = if mods.is_empty() { format!("\x1bO{final_byte}").into_bytes() } else { csi(1, final_byte) };
        }
        KeyCode::F(n @ 5..=20) => out = csi(F_TILDE[usize::from(n - 5)], '~'),
        KeyCode::F(_) => {}
    }
    out
}

/// Encode a mouse event for the pane, with `col`/`row` relative to the pane (0-based). Returns
/// None when the pane did not ask for this kind of event (Move without `mouse_motion`, Drag
/// without `mouse_drag`/`mouse_motion`, anything when no mouse mode is on), or when the
/// coordinates do not fit the encoding. Uses SGR if `sgr_mouse`, else UTF-8 (1005) if
/// `utf8_mouse`, else X10 normal (`ESC [ M Cb Cx Cy`, coordinates up to 222).
pub fn encode_mouse(ev: &MouseEvent, col: u16, row: u16, modes: PaneInputModes) -> Option<Vec<u8>> {
    let wanted = match ev.kind {
        MouseKind::Move => modes.mouse_motion,
        MouseKind::Drag(_) => modes.mouse_drag || modes.mouse_motion,
        _ => modes.mouse_click || modes.mouse_drag || modes.mouse_motion,
    };
    if !wanted {
        return None;
    }
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let mut cb: u32 = match ev.kind {
        MouseKind::Press(b) | MouseKind::Release(b) => button(b),
        MouseKind::Drag(b) => button(b) + 32,
        MouseKind::Move => 3 + 32,
        MouseKind::ScrollUp => 64,
        MouseKind::ScrollDown => 65,
        MouseKind::ScrollLeft => 66,
        MouseKind::ScrollRight => 67,
    };
    cb += 4 * u32::from(ev.mods.shift) + 8 * u32::from(ev.mods.alt) + 16 * u32::from(ev.mods.ctrl);
    let release = matches!(ev.kind, MouseKind::Release(_));
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    if modes.sgr_mouse {
        let final_byte = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{cb};{x};{y}{final_byte}").into_bytes());
    }
    if release {
        cb |= 3;
    }
    let mut out = b"\x1b[M".to_vec();
    for v in [cb + 32, x + 32, y + 32] {
        if modes.utf8_mouse {
            let c = char::from_u32(v).filter(|_| v < 0x800)?;
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        } else {
            out.push(u8::try_from(v).ok()?);
        }
    }
    Some(out)
}

/// Arrow key bytes for a wheel event when the pane enabled alternate scroll (`?1007`), is on the
/// alternate screen and has no mouse mode on; None otherwise. One arrow per event.
pub fn encode_alternate_scroll(ev: &MouseEvent, modes: PaneInputModes) -> Option<Vec<u8>> {
    if !(modes.alternate_scroll && modes.alt_screen) || modes.mouse_click || modes.mouse_drag || modes.mouse_motion {
        return None;
    }
    let code = match ev.kind {
        MouseKind::ScrollUp => KeyCode::Up,
        MouseKind::ScrollDown => KeyCode::Down,
        _ => return None,
    };
    Some(encode_key_code(code, Mods::NONE, modes))
}

/// Wrap pasted bytes for the pane: with `bracketed_paste`, add `ESC[200~`/`ESC[201~` (any
/// `ESC[201~` inside the content is removed so it cannot end the paste early); without it,
/// convert line endings (`\n` and `\r\n`) to `\r`.
pub fn encode_paste(data: &[u8], modes: PaneInputModes) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + PASTE_START.len() + PASTE_END.len());
    if modes.bracketed_paste {
        out.extend_from_slice(PASTE_START);
        let body = out.len();
        for &b in data {
            out.push(b);
            // Checked after every byte so that removing one marker cannot form another.
            if out.len() - body >= PASTE_END.len() && out.ends_with(PASTE_END) {
                out.truncate(out.len() - PASTE_END.len());
            }
        }
        out.extend_from_slice(PASTE_END);
    } else {
        for (i, &b) in data.iter().enumerate() {
            match b {
                b'\n' if i > 0 && data[i - 1] == b'\r' => {}
                b'\n' => out.push(b'\r'),
                _ => out.push(b),
            }
        }
    }
    out
}

/// A key chord parsed from config strings like "ctrl+b", "alt+h", "alt+shift+left", "%", "|",
/// "shift+tab", "f5", "enter", "space", "esc", "pageup", "ctrl+space", "alt+[", "?".
///
/// Normalised form: for [`KeyCode::Char`], shift is never stored (it only uppercases a letter
/// without ctrl: "shift+n" is "N"), and ctrl+letter is lowercase; Shift+Tab is
/// [`KeyCode::BackTab`] with shift.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeySpec {
    pub code: KeyCode,
    pub mods: Mods,
}

/// Error returned by [`KeySpec::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpecError(String);

impl fmt::Display for KeySpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeySpecError {}

impl KeySpec {
    /// Build a chord, normalised like [`KeySpec::parse`] does.
    pub fn new(code: KeyCode, mut mods: Mods) -> KeySpec {
        let code = match code {
            KeyCode::Char(c) => {
                let c = if mods.ctrl {
                    c.to_ascii_lowercase()
                } else if mods.shift {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                mods.shift = false;
                KeyCode::Char(c)
            }
            KeyCode::Tab if mods.shift => KeyCode::BackTab,
            KeyCode::BackTab => {
                mods.shift = true;
                code
            }
            _ => code,
        };
        KeySpec { code, mods }
    }

    /// Parse a chord: `+`-separated modifiers (`ctrl`/`control`, `alt`/`meta`/`opt`/`option`,
    /// `shift`, case-insensitive) followed by a single character or a key name (`enter`, `tab`,
    /// `backtab`, `backspace`, `esc`, `space`, `plus`, arrows, `home`, `end`, `pageup`,
    /// `pagedown`, `insert`, `delete`, `f1`..`f20`, case-insensitive). A single uppercase letter
    /// like "N" means `Char('N')`; "ctrl++" is Ctrl and the `+` key.
    pub fn parse(s: &str) -> Result<KeySpec, KeySpecError> {
        let err = |reason: String| KeySpecError(format!("invalid key {s:?}: {reason}"));
        let s = s.trim();
        let (prefix, key) = if s == "+" {
            ("", "+")
        } else if let Some(prefix) = s.strip_suffix("++") {
            (prefix, "+")
        } else {
            s.rsplit_once('+').unwrap_or(("", s))
        };
        let mut mods = Mods::NONE;
        if !prefix.is_empty() {
            for m in prefix.split('+') {
                match m.trim().to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => mods.ctrl = true,
                    "alt" | "meta" | "opt" | "option" => mods.alt = true,
                    "shift" => mods.shift = true,
                    "" => return Err(err("empty modifier".into())),
                    other => return Err(err(format!("unknown modifier {other:?}"))),
                }
            }
        }
        let key = key.trim();
        let mut chars = key.chars();
        let code = match (chars.next(), chars.next()) {
            (None, _) => return Err(err("missing key".into())),
            (Some(c), None) => KeyCode::Char(c),
            _ => named_key(&key.to_ascii_lowercase()).ok_or_else(|| err(format!("unknown key name {key:?}")))?,
        };
        Ok(KeySpec::new(code, mods))
    }

    /// Whether a received key matches this chord. Alt must match. For characters, shift is
    /// ignored (the character already reflects it) and Ctrl+letter matches case-insensitively;
    /// other keys need the exact modifiers. Chords that the legacy encoding cannot tell apart
    /// also match each other (Ctrl+I and Tab, Ctrl+[ and Esc, Ctrl+Space and Ctrl+@,
    /// Ctrl+Backspace and Ctrl+H).
    pub fn matches(&self, key: &Key) -> bool {
        if self.mods.alt != key.mods.alt {
            return false;
        }
        let direct = match (self.code, key.code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => {
                self.mods.ctrl == key.mods.ctrl && if self.mods.ctrl { a.eq_ignore_ascii_case(&b) } else { a == b }
            }
            (a, b) => {
                a == b
                    && self.mods.ctrl == key.mods.ctrl
                    && (a == KeyCode::BackTab || self.mods.shift == key.mods.shift)
            }
        };
        direct
            || matches!(
                (legacy_byte(self.code, self.mods), legacy_byte(key.code, key.mods)),
                (Some(x), Some(y)) if x == y
            )
    }
}

impl FromStr for KeySpec {
    type Err = KeySpecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        KeySpec::parse(s)
    }
}

impl fmt::Display for KeySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.mods.alt {
            f.write_str("alt+")?;
        }
        if self.mods.shift || self.code == KeyCode::BackTab {
            f.write_str("shift+")?;
        }
        let name = match self.code {
            KeyCode::Char(' ') => "space",
            KeyCode::Char(c) => return write!(f, "{c}"),
            KeyCode::Enter => "enter",
            KeyCode::Tab | KeyCode::BackTab => "tab",
            KeyCode::Backspace => "backspace",
            KeyCode::Esc => "esc",
            KeyCode::Up => "up",
            KeyCode::Down => "down",
            KeyCode::Left => "left",
            KeyCode::Right => "right",
            KeyCode::Home => "home",
            KeyCode::End => "end",
            KeyCode::PageUp => "pageup",
            KeyCode::PageDown => "pagedown",
            KeyCode::Insert => "insert",
            KeyCode::Delete => "delete",
            KeyCode::F(n) => return write!(f, "f{n}"),
        };
        f.write_str(name)
    }
}

/// Key for a lowercase key name.
fn named_key(name: &str) -> Option<KeyCode> {
    Some(match name {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" | "bspace" | "bs" => KeyCode::Backspace,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "plus" => KeyCode::Char('+'),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "insert" | "ins" => KeyCode::Insert,
        "delete" | "del" => KeyCode::Delete,
        _ => {
            let n: u8 = name.strip_prefix('f')?.parse().ok()?;
            if !(1..=20).contains(&n) {
                return None;
            }
            KeyCode::F(n)
        }
    })
}

/// The single byte a chord produces in the legacy encoding, ignoring alt, if it is one of the
/// bytes shared by several chords.
fn legacy_byte(code: KeyCode, mods: Mods) -> Option<u8> {
    match (code, mods.ctrl, mods.shift) {
        (KeyCode::Char(c), true, _) => control_byte(c),
        (KeyCode::Backspace, true, false) => Some(0x08),
        (KeyCode::Backspace, false, false) => Some(0x7f),
        (KeyCode::Tab, false, false) => Some(b'\t'),
        (KeyCode::Enter, false, false) => Some(b'\r'),
        (KeyCode::Esc, false, false) => Some(ESC),
        _ => None,
    }
}

#[cfg(test)]
mod kitty_tests {
    use super::*;

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
}

#[cfg(test)]
mod tests {
    use super::KeyCode::*;
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

    fn parse_all(bytes: &[u8]) -> Vec<InputEvent> {
        InputParser::new().feed(bytes)
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
        assert_eq!(parser.feed(b"\x1b"), vec![key_ev(Esc, NONE, b"\x1b")]);
        assert!(!parser.has_pending());
        assert_eq!(parser.feed(b"a\x1b"), vec![key_ev(Char('a'), NONE, b"a"), key_ev(Esc, NONE, b"\x1b")]);
        // Documented consequence: a sequence split right after its ESC is not reassembled.
        assert_eq!(parser.feed(b"\x1b"), vec![key_ev(Esc, NONE, b"\x1b")]);
        assert_eq!(parser.feed(b"[A"), vec![key_ev(Char('['), NONE, b"["), key_ev(Char('A'), SHIFT, b"A")]);
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
        // Release events and kitty functional keys are not plain keys.
        assert_eq!(parse_all(b"\x1b[97;5:3u"), vec![raw_ev(b"\x1b[97;5:3u")]);
        assert_eq!(parse_all(b"\x1b[57399u"), vec![raw_ev(b"\x1b[57399u")]);
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
        assert_eq!(
            feed_chunks(&[b"\x1b[200~a\x1b[20", b"2~b\x1b[201~"]),
            vec![InputEvent::Paste(b"a\x1b[202~b".to_vec())]
        );

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
        assert_eq!(parser.flush(), vec![InputEvent::Paste(b"abc".to_vec())]);
        assert!(!parser.has_pending());
        assert!(parser.flush().is_empty());
        // Still inside the paste: the marker prefix was kept.
        assert_eq!(parser.feed(b"01~"), vec![]);
        assert_eq!(parser.feed(b"q"), vec![key_ev(Char('q'), NONE, b"q")]);

        let mut parser = InputParser::new();
        parser.feed(b"\x1b[200~abc");
        parser.flush();
        assert_eq!(parser.feed(b"\x01def\x1b[201~"), vec![InputEvent::Paste(b"\x01def".to_vec())]);
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
    fn split_at_every_offset_matches_single_chunk() {
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
    fn split_into_three_chunks_matches_single_chunk() {
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
}
