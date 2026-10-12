//! Encoding keys, mouse events and pastes the way a pane's program asked for them.

use super::{ESC, F_TILDE, Key, KeyCode, Mods, MouseButton, MouseEvent, MouseKind, PASTE_END, PASTE_START};

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
        && let Some(bytes) = encode_kitty_key(key, modes.kitty_flags)
    {
        return bytes;
    }
    let cursor_key = matches!(key.code, Up | Down | Left | Right | Home | End) && key.mods.is_empty();
    let keypad = !modes.app_keypad && key.raw.starts_with(b"\x1bO") && matches!(key.code, Char(_) | Enter);
    if key.raw.is_empty() || cursor_key || keypad || is_extended_encoding(&key.raw) || has_lock_bits(&key.raw) {
        // Ctrl with a letter of a non-Latin layout sends the control byte of the letter at its
        // place, as terminals do without the protocol.
        let code = match key.code {
            Char(c) if key.mods.ctrl && control_byte(c).is_none() => key
                .by_place()
                .map(|k| k.code)
                .filter(|code| matches!(code, Char(b) if b.is_ascii_alphabetic()))
                .unwrap_or(key.code),
            code => code,
        };
        encode_key_code(code, key.mods, modes)
    } else {
        key.raw.clone()
    }
}

/// The key code and modifier field of the kitty keyboard protocol sequence for a pane that
/// enabled it: for the keys whose legacy encoding is ambiguous or lossy (Shift+Enter, Ctrl+Enter,
/// Alt+letters, Esc…), and for every key with flag 8. `None` means the legacy encoding is also
/// what the protocol sends (plain text, arrows, function keys).
fn kitty_code(code: KeyCode, mods: Mods, flags: u8) -> Option<(u32, u32)> {
    use KeyCode::*;
    let all_keys = flags & 8 != 0;
    let bits = u32::from(mods.shift) | (u32::from(mods.alt) << 1) | (u32::from(mods.ctrl) << 2);
    let m = 1 + bits;
    match code {
        Esc => Some((27, m)),
        Enter | Tab | Backspace if all_keys || m != 1 => {
            let cp = match code {
                Enter => 13,
                Tab => 9,
                _ => 127,
            };
            Some((cp, m))
        }
        BackTab if all_keys => Some((9, 1 + (bits | 1))),
        Char(c) if all_keys || mods.ctrl || mods.alt => {
            // The key is reported unshifted, with shift as a modifier.
            let base = if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c };
            let m = if c.is_ascii_uppercase() { 1 + (bits | 1) } else { m };
            Some((base as u32, m))
        }
        _ => None,
    }
}

/// The kitty sequence of `key` ([`kitty_code`]), with what the pane's flags ask for beyond the
/// key itself: its alternate keys (flag 4) and the text it types (flags 8 and 16). A key the
/// terminal sent with the protocol goes on with the code and alternates the terminal gave;
/// another one is described from what it is. The modifiers are Shift, Alt and Ctrl, without the
/// lock keys, as for every program.
fn encode_kitty_key(key: &Key, flags: u8) -> Option<Vec<u8>> {
    let (cp, m) = kitty_code(key.code, key.mods, flags)?;
    let (cp, m, shifted, base, text) = match super::decode::kitty_fields(&key.raw) {
        // The modifiers as the terminal sent them (Alt that typed a character included), without
        // the lock keys. A program that does not get the text learns the case from Shift.
        Some(f) if f.key != 0 => {
            let gets_text = flags & (8 | 16) == 8 | 16 && !f.text.is_empty();
            let shift = if gets_text { f.mods & 1 } else { u32::from(key.mods.shift) };
            (f.key, 1 + ((f.mods & 62) | shift), f.shifted, f.base, f.text)
        }
        _ => match key.code {
            KeyCode::Char(c) => {
                let shifted = if c as u32 == cp { 0 } else { c as u32 };
                let text = if key.mods.ctrl || key.mods.alt { Vec::new() } else { vec![c] };
                (cp, m, shifted, 0, text)
            }
            _ => (cp, m, 0, 0, Vec::new()),
        },
    };
    let mut out = format!("\x1b[{cp}");
    if flags & 4 != 0 && (shifted != 0 || base != 0) {
        out.push(':');
        if shifted != 0 {
            out.push_str(&shifted.to_string());
        }
        if base != 0 {
            out.push_str(&format!(":{base}"));
        }
    }
    let text: Vec<String> =
        if flags & (8 | 16) == 8 | 16 { text.iter().map(|c| (*c as u32).to_string()).collect() } else { Vec::new() };
    if m != 1 || !text.is_empty() {
        out.push_str(&format!(";{m}"));
    }
    if !text.is_empty() {
        out.push_str(&format!(";{}", text.join(":")));
    }
    out.push('u');
    Some(out.into_bytes())
}

/// Whether `raw` is a legacy CSI key, possibly ESC-prefixed (Alt), whose modifier carries Caps
/// Lock or Num Lock (kitty bits 64 and 128, as Ghostty sends them): programs expecting xterm's
/// modifiers would not know it.
fn has_lock_bits(raw: &[u8]) -> bool {
    let raw = match raw.strip_prefix(&[ESC]) {
        Some(rest) if rest.starts_with(&[ESC]) => rest,
        _ => raw,
    };
    let Some(body) = raw.strip_prefix(b"\x1b[") else { return false };
    let Some((_, params)) = body.split_last() else { return false };
    let Some(modifier) = params.split(|b| *b == b';').nth(1) else { return false };
    let first = modifier.split(|b| *b == b':').next().unwrap_or_default();
    std::str::from_utf8(first).ok().and_then(|m| m.parse::<u32>().ok()).is_some_and(|m| m > 16)
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
pub(super) fn control_byte(c: char) -> Option<u8> {
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
    encode_paste_after(data, modes, false)
}

/// [`encode_paste`] for the next piece of a paste whose previous piece ended with CR
/// (`after_cr`): an LF opening this one completes that line end rather than adding another.
pub fn encode_paste_after(data: &[u8], modes: PaneInputModes, after_cr: bool) -> Vec<u8> {
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
            let after = if i == 0 { after_cr } else { data[i - 1] == b'\r' };
            match b {
                b'\n' if after => {}
                b'\n' => out.push(b'\r'),
                _ => out.push(b),
            }
        }
    }
    out
}
