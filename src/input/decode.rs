//! Parsing what the client's terminal sends into [`InputEvent`]s.
//!
//! Known ambiguities of the legacy encoding: `ESC [ 1 ; m R` is read as a modified F3 even though
//! a cursor position report for row 1 looks the same, and a chunk ending right after `ESC [`,
//! `ESC O`, `ESC ]`, `ESC P`, `ESC _`, `ESC ^` or `ESC X` stays pending until [`InputParser::flush`]
//! turns it into the corresponding Alt+key.
//!
//! A chunk is one read of the client's terminal, carried whole in one frame (also over ssh). A
//! read can still end between ESC and the rest of a sequence, so a chunk ending with a lone ESC
//! waits for the next chunk or for [`InputParser::flush`] before it is the Esc key.

use super::{ESC, F_TILDE, InputEvent, Key, KeyCode, Mods, MouseButton, MouseEvent, MouseKind, PASTE_END};

/// What a kitty keyboard protocol key (`CSI key:shifted:base ; mods:event ; text u`) says, as
/// received: the key's own code (unshifted), the one it gives with Shift, the key at its place on
/// a US keyboard, and the text it typed. Absent fields are 0 or empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KittyFields {
    pub key: u32,
    pub shifted: u32,
    pub base: u32,
    /// The modifier bits as the terminal sent them (Shift 1, Alt 2, Ctrl 4, Super 8, Hyper 16,
    /// Meta 32, Caps Lock 64, Num Lock 128).
    pub mods: u32,
    pub text: Vec<char>,
}

/// The fields of `raw` when it is a kitty protocol key (possibly after an Alt ESC).
pub(super) fn kitty_fields(raw: &[u8]) -> Option<KittyFields> {
    let raw = match raw.strip_prefix(&[ESC]) {
        Some(rest) if rest.starts_with(&[ESC]) => rest,
        _ => raw,
    };
    let body = raw.strip_prefix(b"\x1b[")?.strip_suffix(b"u")?;
    if !body.first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let params = parse_params(body)?;
    if params.is_empty() || params.len() > 3 {
        return None;
    }
    Some(KittyFields {
        key: param(&params, 0),
        shifted: sub_param(&params, 0, 1),
        base: sub_param(&params, 0, 2),
        mods: param(&params, 1).saturating_sub(1),
        text: associated_text(&params).unwrap_or_default(),
    })
}

/// The text a kitty protocol key typed (its third field), when it is printable text.
fn associated_text(params: &[Vec<u32>]) -> Option<Vec<char>> {
    let field = params.get(2).filter(|f| f.iter().any(|&cp| cp != 0))?;
    field.iter().map(|&cp| char::from_u32(cp).filter(|c| !c.is_control())).collect()
}

const BEL: u8 = 0x07;
/// Longest CSI sequence accepted before giving up and emitting it as raw bytes.
pub(super) const MAX_CSI_LEN: usize = 256;
/// Longest OSC/DCS/APC/PM/SOS string accepted before giving up and emitting it as raw bytes.
const MAX_STRING_LEN: usize = 64 * 1024;

/// Incremental parser for the bytes the client forwards from its terminal.
#[derive(Debug, Default)]
pub struct InputParser {
    /// Bytes not consumed yet (an incomplete sequence, or a possible paste end marker prefix).
    buf: Vec<u8>,
    /// Content of the bracketed paste in progress, if any.
    paste: Option<Vec<u8>>,
    /// Pieces of the paste in progress went out already: its end is a `Paste`, even an empty one.
    pieces: bool,
    /// Last pressed button, to resolve X10 releases (which do not say which button).
    last_button: Option<MouseButton>,
}

impl InputParser {
    /// An empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a chunk read from the client. Complete events are returned; an incomplete escape
    /// sequence at the end, a lone ESC included, is kept for the next call or the flush.
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

    /// Whether all it was fed was read whole: no sequence cut in two, no paste still coming.
    pub fn idle(&self) -> bool {
        self.buf.is_empty() && self.paste.is_none()
    }

    /// Called after a short timeout when [`has_pending`](Self::has_pending): turn whatever is
    /// pending into events. `ESC` + one printable byte (e.g. `ESC [`) becomes Alt+that key, other
    /// partial sequences become [`InputEvent::Raw`]. Inside a bracketed paste, the content
    /// received so far is emitted as a `PastePiece` and the parser keeps waiting for the end marker.
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
                    // The end of a paste that went out in pieces is told even when nothing is left.
                    let pieces = std::mem::take(&mut self.pieces);
                    if !content.is_empty() || pieces {
                        events.push(InputEvent::Paste(std::mem::take(content)));
                    }
                    self.paste = None;
                    pos += end + PASTE_END.len();
                    continue;
                }
                // Keep a possible prefix of the end marker for the next chunk.
                let take = rest.len() - partial_suffix(rest, PASTE_END);
                content.extend_from_slice(&rest[..take]);
                pos += take;
                // A paste that keeps coming goes out in pieces, as one that pauses does: what
                // waits for its end marker here stays bounded.
                if (flush || content.len() >= PASTE_PIECE) && !content.is_empty() {
                    events.push(InputEvent::PastePiece(std::mem::take(content)));
                    self.pieces = true;
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
                // Even a lone ESC waits for the flush: a read can end between ESC and the rest of
                // a sequence (`[200~` starting a paste), which must not become Escape.
                Step::Incomplete => {
                    if flush {
                        events.push(flush_partial(rest));
                        pos = self.buf.len();
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
            Tok::Keys(keys) => events.extend(keys),
            Tok::PasteStart => self.paste = Some(Vec::new()),
            Tok::X10Release(mut ev) => {
                let button = self.last_button.take().unwrap_or(MouseButton::Left);
                ev.kind = MouseKind::Release(button);
                events.push(InputEvent::Mouse(ev));
            }
        }
    }
}

/// Size at which an unfinished bracketed paste is passed on without waiting for its end.
const PASTE_PIECE: usize = 1 << 20;

/// What a parse step produced.
enum Tok {
    Event(InputEvent),
    /// Several keys from one sequence: the text of a kitty protocol key, one key per character.
    Keys(Vec<InputEvent>),
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

/// Modifier parameter of a legacy key, None if outside xterm's range (likely a report). The
/// kitty protocol adds Caps Lock (64) and Num Lock (128): with Num Lock on, Ghostty sends Up as
/// `CSI 1;129 A`. `locks` is false where a report looks the same (`CSI 1;129 R` is a cursor
/// position, not F3).
fn legacy_mods(params: &[Vec<u32>], i: usize, locks: bool) -> Option<Mods> {
    let m = param(params, i);
    let ignored = if locks { 64 | 128 } else { 0 };
    (m <= 256 && (m.saturating_sub(1) & !ignored) < 16).then(|| Mods::from_xterm_param(m))
}

/// A key release (kitty event type 3): only programs that asked for releases want it.
fn is_release(params: &[Vec<u32>]) -> bool {
    sub_param(params, 1, 1) == 3
}

fn release_tok(seq: &[u8]) -> Tok {
    Tok::Event(InputEvent::KittyKey { raw: seq.to_vec(), release: true })
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
    if code.is_some() && is_release(params) {
        return release_tok(seq);
    }
    match (code, legacy_mods(params, 1, final_byte != b'R')) {
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
    if is_release(params) {
        return release_tok(seq);
    }
    match legacy_mods(params, 1, true) {
        Some(mods) if params.len() <= 2 => key_tok(code, mods, seq),
        _ => raw_tok(seq),
    }
}

/// `CSI codepoint[:shifted[:base]] [; mods[:event] [; text]] u` (kitty / fixterms).
fn csi_u(params: &[Vec<u32>], seq: &[u8]) -> Tok {
    // Release events (event type 3) are not key presses.
    if params.is_empty() || params.len() > 3 {
        return raw_tok(seq);
    }
    if is_release(params) {
        return release_tok(seq);
    }
    let mods = Mods::from_xterm_param(param(params, 1));
    // What the key typed wins over its code: a dead key's accent, AltGr, the macOS Option key
    // and input methods show only there.
    if let Some(text) = associated_text(params) {
        // Text with no key (an input method's commit) is text, not key presses: it goes in as
        // pasted text would, and the leader menu does not read it as keys.
        if param(params, 0) == 0 {
            return Tok::Event(InputEvent::Paste(text.iter().collect::<String>().into_bytes()));
        }
        return text_tok(&text, mods, seq);
    }
    codepoint_key(param(params, 0), sub_param(params, 0, 1), mods, seq)
}

/// The keys for the text a kitty protocol key typed. One character is that key, as received
/// (Alt is part of the text then: the macOS Option key types characters); several are typed
/// text, one key each.
fn text_tok(text: &[char], mods: Mods, seq: &[u8]) -> Tok {
    match text {
        // The text says what was typed, Caps Lock included: for a letter its case wins over Shift.
        [c] => {
            let shift = if c.is_alphabetic() { c.is_uppercase() } else { mods.shift };
            key_tok(KeyCode::Char(*c), Mods { alt: false, shift, ..mods }, seq)
        }
        _ => Tok::Keys(
            text.iter()
                .map(|c| {
                    InputEvent::Key(make_key(KeyCode::Char(*c), Mods::NONE, c.encode_utf8(&mut [0; 4]).as_bytes()))
                })
                .collect(),
        ),
    }
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
                Some(c) if ('\u{e000}'..='\u{f8ff}').contains(&c) => match keypad(cp) {
                    Some(code) => code,
                    None => return Tok::Event(InputEvent::KittyKey { raw: seq.to_vec(), release: false }),
                },
                Some(c) if !c.is_control() => KeyCode::Char(c),
                _ => return raw_tok(seq),
            }
        }
    };
    key_tok(code, mods, seq)
}

/// The keypad keys, which the kitty protocol reports with codes of their own (kraal asks the
/// terminal for it): as the keys they type.
fn keypad(cp: u32) -> Option<KeyCode> {
    use KeyCode::*;
    Some(match cp {
        57399..=57408 => Char(char::from_digit(cp - 57399, 10)?),
        57409 => Char('.'),
        57410 => Char('/'),
        57411 => Char('*'),
        57412 => Char('-'),
        57413 => Char('+'),
        57414 => Enter,
        57415 => Char('='),
        57416 => Char(','),
        57417 => Left,
        57418 => Right,
        57419 => Up,
        57420 => Down,
        57421 => PageUp,
        57422 => PageDown,
        57423 => Home,
        57424 => End,
        57425 => Insert,
        57426 => Delete,
        _ => return None,
    })
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
