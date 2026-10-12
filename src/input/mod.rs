//! Terminal input: parsing what the client's terminal sends, and encoding it for panes.
//!
//! The client puts the outer terminal in raw mode with SGR mouse reporting, bracketed paste and
//! focus events enabled, and forwards its stdin bytes verbatim. [`InputParser`] turns that stream
//! into [`InputEvent`]s: legacy xterm keys, SGR mouse reports, pastes, focus changes, and (when
//! tolerated) CSI u / `modifyOtherKeys` keys. Terminal responses to queries (OSC/DCS/APC/PM/SOS
//! strings, CSI reports) come out as [`InputEvent::Raw`]. The `encode_*` functions produce what a
//! pane program expects given the modes it enabled ([`PaneInputModes`]), and [`KeySpec`] parses
//! and matches keybindings from the configuration.

mod decode;
mod encode;
#[cfg(test)]
mod fuzz;
mod keyspec;
#[cfg(test)]
mod tests;

pub use decode::InputParser;
pub use encode::{PaneInputModes, encode_alternate_scroll, encode_key, encode_mouse, encode_paste, encode_paste_after};
pub use keyspec::KeySpec;

const ESC: u8 = 0x1b;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
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
    /// Alt alone.
    pub const ALT: Mods = Mods { shift: false, alt: true, ctrl: false };
    /// Ctrl alone.
    pub const CTRL: Mods = Mods { shift: false, alt: false, ctrl: true };

    /// True when no modifier is held.
    fn is_empty(self) -> bool {
        !(self.shift || self.alt || self.ctrl)
    }

    /// Decode an xterm modifier parameter (`1 + bits`: shift 1, alt 2, ctrl 4, meta 8).
    /// Meta is folded into alt; higher bits (kitty's hyper, caps/num lock) are ignored.
    fn from_xterm_param(param: u32) -> Mods {
        let bits = param.saturating_sub(1);
        Mods { shift: bits & 1 != 0, alt: bits & (2 | 8) != 0, ctrl: bits & 4 != 0 }
    }

    /// The xterm modifier parameter for these modifiers (`1` when none).
    fn xterm_param(self) -> u32 {
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
    /// Whether the terminal said where this key is (kitty keyboard protocol, alternate keys).
    pub fn has_place(&self) -> bool {
        decode::kitty_fields(&self.raw).is_some_and(|f| f.base != 0)
    }

    /// The key at the same place on a US keyboard, when the terminal said where the key is
    /// (kitty keyboard protocol, alternate keys) and a keymap should read it that way: on the
    /// digit row without Shift (AZERTY's `&` is `1`), and on a non-Latin layout (Russian `и` is
    /// `b`). Modifiers stay. None for a Latin letter: it is the letter it types, wherever it is.
    pub fn by_place(&self) -> Option<Key> {
        let KeyCode::Char(c) = self.code else { return None };
        let base = decode::kitty_fields(&self.raw).and_then(|f| char::from_u32(f.base))?;
        let code = if base.is_ascii_digit() {
            if self.mods.shift || c.is_ascii_digit() {
                return None;
            }
            base
        } else if base.is_ascii_alphabetic() && !c.is_ascii() {
            // The case of what was typed, Caps Lock included.
            if c.is_uppercase() && !self.mods.ctrl { base.to_ascii_uppercase() } else { base.to_ascii_lowercase() }
        } else {
            return None;
        };
        Some(Key { code: KeyCode::Char(code), mods: self.mods, raw: self.raw.clone() })
    }
}

/// The kitty keyboard flag a program must have enabled to get this [`InputEvent::KittyKey`]: 2
/// for a release, 8 for a modifier or lock key pressed alone, 0 (any flag) for the other keys
/// (Print Screen, media keys).
pub fn kitty_key_flag(raw: &[u8], release: bool) -> u8 {
    if release {
        return 2;
    }
    match decode::kitty_fields(raw).map(|f| f.key) {
        Some(57358..=57360 | 57441..=57454) => 8,
        _ => 0,
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
    /// Content of a bracketed paste, without the markers. Never empty, except as the end of a
    /// paste that came in pieces.
    Paste(Vec<u8>),
    /// Part of a bracketed paste still coming (a long one, or one slow to arrive): more pieces
    /// follow, then a `Paste` with the last part.
    PastePiece(Vec<u8>),
    FocusGained,
    FocusLost,
    /// Bytes that are not a recognised key/mouse/paste/focus event (terminal responses, unknown
    /// sequences, invalid UTF-8), to forward verbatim or inspect.
    Raw(Vec<u8>),
    /// A key only the kitty keyboard protocol can express (Print Screen, a media key, a lock
    /// key) or a key release, as received: only a program that enabled the protocol (and asked
    /// for releases) understands it.
    KittyKey {
        raw: Vec<u8>,
        release: bool,
    },
}
