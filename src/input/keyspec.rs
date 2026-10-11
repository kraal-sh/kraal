//! Keys as the configuration writes them (`ctrl+b`, `alt+left`, `F5`), parsed and matched.

use std::fmt;
use std::str::FromStr;

use super::encode::control_byte;
use super::{ESC, Key, KeyCode, Mods};

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
