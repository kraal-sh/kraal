//! Escape sequences kraal reads from pane output besides the emulator: notifications, the
//! working directory (OSC 7), the clipboard (OSC 52).

use std::path::PathBuf;

pub(super) fn sanitize_title(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(120).collect()
}

/// OSC sequences kraal itself cares about, picked out of the output stream.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Osc {
    Notify { title: String, body: String },
    Cwd { host: String, path: PathBuf },
}

#[derive(Default)]
pub(super) struct OscScanner {
    state: ScanState,
    buf: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq)]
pub(super) enum ScanState {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

impl OscScanner {
    pub(super) fn scan(&mut self, bytes: &[u8]) -> Vec<Osc> {
        let mut out = Vec::new();
        for &b in bytes {
            self.state = match (self.state, b) {
                (ScanState::Ground, 0x1b) => ScanState::Esc,
                (ScanState::Ground, _) => ScanState::Ground,
                (ScanState::Esc, b']') => {
                    self.buf.clear();
                    ScanState::Osc
                }
                (ScanState::Esc, 0x1b) => ScanState::Esc,
                (ScanState::Esc, _) => ScanState::Ground,
                (ScanState::Osc, 0x07) => {
                    out.extend(parse_osc(&self.buf));
                    ScanState::Ground
                }
                (ScanState::Osc, 0x1b) => ScanState::OscEsc,
                (ScanState::Osc, b) => {
                    if self.buf.len() < 8192 {
                        self.buf.push(b);
                    }
                    ScanState::Osc
                }
                (ScanState::OscEsc, b'\\') => {
                    out.extend(parse_osc(&self.buf));
                    ScanState::Ground
                }
                (ScanState::OscEsc, b']') => {
                    self.buf.clear();
                    ScanState::Osc
                }
                (ScanState::OscEsc, _) => ScanState::Ground,
            };
        }
        out
    }
}

pub(super) fn parse_osc(data: &[u8]) -> Option<Osc> {
    let s = String::from_utf8_lossy(data);
    let (code, rest) = s.split_once(';')?;
    match code {
        // iTerm2 style: OSC 9 ; message. `9;4;…` is a progress report, not a notification.
        "9" if !rest.starts_with("4;") => Some(Osc::Notify { title: String::new(), body: clean(rest) }),
        // rxvt/Ghostty style: OSC 777 ; notify ; title ; body
        "777" => {
            let rest = rest.strip_prefix("notify;")?;
            let (title, body) = rest.split_once(';').unwrap_or((rest, ""));
            Some(Osc::Notify { title: clean(title), body: clean(body) })
        }
        "7" => {
            let url = rest.strip_prefix("file://")?;
            let slash = url.find('/')?;
            let path = &url[slash..];
            Some(Osc::Cwd { host: url[..slash].to_string(), path: PathBuf::from(percent_decode(path)) })
        }
        _ => None,
    }
}

pub(super) fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(300).collect()
}

pub(super) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escape sequence asking the outer terminal to copy `text` (OSC 52).
pub fn osc52(text: &str) -> Vec<u8> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07").into_bytes()
}

/// Escape sequence for a desktop notification on the outer terminal.
pub fn osc_notify(title: &str, body: &str) -> Vec<u8> {
    let t = clean(title).replace(';', ",");
    let b = clean(body).replace(';', ",");
    format!("\x1b]777;notify;{t};{b}\x07\x1b]9;{t}: {b}\x07").into_bytes()
}
