//! Escape sequences kraal reads from pane output besides the emulator: notifications, the
//! working directory (OSC 7), the shell's command marks (OSC 133), the clipboard (OSC 52), and
//! kraal's own (OSC 7701).

use std::path::PathBuf;

use super::commands::Mark;
use crate::context::RemoteContexts;

/// What kraal answers a shell that asks whether its terminal is kraal (`OSC 7701 ; ?`): the
/// shell integration asks over SSH, where kraal's variables do not reach.
pub(super) const KRAAL_ANSWER: &str = "\x1b]7701;kraal\x07";

pub(super) fn sanitize_title(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() && !crate::text::invisible(*c)).take(120).collect()
}

/// OSC sequences kraal itself cares about, picked out of the output stream. `Query` asks whether
/// the terminal is kraal (`OSC 7701 ; ?`); `Contexts` is what a shell works on
/// (`OSC 7701 ; context ; k8s=… ; ns=… ; tf=… ; aws=…`). `Attached` is a kraal client attached
/// to a server's session saying so (`OSC 7701 ; attach ; <server> ; <session>`, nothing after
/// `attach` once it is not), and `Shows` the pane of that session it shows
/// (`OSC 7701 ; shows ; <pane id>`), which that session tells it.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Osc {
    Notify { title: String, body: String },
    Cwd { host: String, path: PathBuf },
    Mark(Mark),
    Query,
    Contexts(RemoteContexts),
    Attached(Option<(String, String)>),
    Shows(Option<u32>),
}

#[derive(Default)]
pub(super) struct OscScanner {
    state: ScanState,
    buf: Vec<u8>,
    /// What an OSC 9 says of the OSC 777 notification that just ended, until anything else
    /// comes: a program that sends both for one notification (kraal does) is heard once.
    echo: Option<String>,
}

#[derive(Default, Clone, Copy, PartialEq)]
pub(super) enum ScanState {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

/// Longest OSC body kept; the rest is dropped.
const MAX_OSC: usize = 8192;

impl OscScanner {
    /// The sequences that end in `bytes`, each with the offset just past its end. Text and the
    /// body of a sequence are passed over in runs, to the next byte that matters.
    pub(super) fn scan(&mut self, bytes: &[u8]) -> Vec<(usize, Osc)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                ScanState::Ground => {
                    let next = memchr::memchr(0x1b, &bytes[i..]);
                    // Something between a notification and the next sequence.
                    if next != Some(0) {
                        self.echo = None;
                    }
                    let Some(n) = next else { break };
                    i += n + 1;
                    self.state = ScanState::Esc;
                }
                ScanState::Osc => {
                    let rest = &bytes[i..];
                    let end = memchr::memchr2(0x07, 0x1b, rest);
                    let body = &rest[..end.unwrap_or(rest.len())];
                    let room = MAX_OSC.saturating_sub(self.buf.len());
                    self.buf.extend_from_slice(&body[..body.len().min(room)]);
                    let Some(n) = end else { break };
                    i += n + 1;
                    self.state = if rest[n] == 0x07 {
                        out.extend(self.finish().map(|o| (i, o)));
                        ScanState::Ground
                    } else {
                        ScanState::OscEsc
                    };
                }
                state => {
                    let b = bytes[i];
                    i += 1;
                    if state == ScanState::Esc && b != b']' {
                        self.echo = None;
                    }
                    self.state = match (state, b) {
                        (ScanState::Esc | ScanState::OscEsc, b']') => {
                            self.buf.clear();
                            ScanState::Osc
                        }
                        (ScanState::Esc, 0x1b) => ScanState::Esc,
                        (ScanState::OscEsc, b'\\') => {
                            out.extend(self.finish().map(|o| (i, o)));
                            ScanState::Ground
                        }
                        _ => ScanState::Ground,
                    };
                }
            }
        }
        out
    }

    /// The sequence just read, unless it is the OSC 9 copy of the notification before it.
    fn finish(&mut self) -> Option<Osc> {
        let echo = self.echo.take();
        let osc = parse_osc(&self.buf)?;
        if let Osc::Notify { title, body } = &osc {
            if self.buf.starts_with(b"777;") {
                self.echo = Some(format!("{title}: {body}"));
            } else if echo.as_deref() == Some(body.as_str()) {
                return None;
            }
        }
        Some(osc)
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
        // FinalTerm marks: `A` prompt, `B` command line, `C` output, `D[;status]` end, each
        // possibly followed by `key=value` options (aid, cl…) that kraal does not need.
        "133" => {
            let mut fields = rest.split(';');
            let mark = match fields.next()? {
                "A" => Mark::Prompt,
                "B" => Mark::Input,
                "C" => Mark::Output,
                "D" => Mark::End(fields.next().and_then(|f| f.trim().parse().ok())),
                _ => return None,
            };
            Some(Osc::Mark(mark))
        }
        // kraal's own: other terminals ignore an OSC number they do not know.
        "7701" => match rest.split_once(';').unwrap_or((rest, "")) {
            ("?", _) => Some(Osc::Query),
            ("context", fields) => Some(Osc::Contexts(RemoteContexts::parse(fields))),
            ("attach", "") => Some(Osc::Attached(None)),
            ("attach", fields) => {
                let (server, session) = fields.split_once(';')?;
                let ok = crate::servers::valid_name(server) && crate::paths::valid_session_name(session);
                ok.then(|| Osc::Attached(Some((server.to_string(), session.to_string()))))
            }
            ("shows", pane) => Some(Osc::Shows(pane.parse().ok())),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() && !crate::text::invisible(*c)).take(300).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The scanner as it was, one byte at a time.
    fn scan_every_byte(s: &mut OscScanner, bytes: &[u8]) -> Vec<(usize, Osc)> {
        let mut out = Vec::new();
        for (i, &b) in bytes.iter().enumerate() {
            let between = match s.state {
                ScanState::Ground => b != 0x1b,
                ScanState::Esc => b != b']',
                _ => false,
            };
            if between {
                s.echo = None;
            }
            s.state = match (s.state, b) {
                (ScanState::Ground, 0x1b) => ScanState::Esc,
                (ScanState::Ground, _) => ScanState::Ground,
                (ScanState::Esc | ScanState::OscEsc, b']') => {
                    s.buf.clear();
                    ScanState::Osc
                }
                (ScanState::Esc, 0x1b) => ScanState::Esc,
                (ScanState::Esc, _) => ScanState::Ground,
                (ScanState::Osc, 0x07) => {
                    out.extend(s.finish().map(|o| (i + 1, o)));
                    ScanState::Ground
                }
                (ScanState::Osc, 0x1b) => ScanState::OscEsc,
                (ScanState::Osc, b) => {
                    if s.buf.len() < MAX_OSC {
                        s.buf.push(b);
                    }
                    ScanState::Osc
                }
                (ScanState::OscEsc, b'\\') => {
                    out.extend(s.finish().map(|o| (i + 1, o)));
                    ScanState::Ground
                }
                (ScanState::OscEsc, _) => ScanState::Ground,
            };
        }
        out
    }

    /// Reading text and sequence bodies in runs finds the same sequences at the same offsets,
    /// however the reads cut them, as reading every byte.
    #[test]
    fn runs_find_what_bytes_find() {
        let long = format!("\x1b]9;{}\x07", "x".repeat(MAX_OSC + 10));
        let pieces: &[&[u8]] = &[
            b"\x1b",
            b"]",
            b"\\",
            b"\x07",
            b"text ",
            b"133;A",
            b"133;D;1",
            b"7;file://h/tmp",
            b"9;hi",
            b";",
            b"7701;?",
            b"777;notify;t;b",
            b"\x1b]133;C\x07",
            b"\x1b]7;file://host/a%20b\x1b\\",
            b"\x1b]777;notify;t;b\x07",
            b"\x1b]9;t: b\x07",
            b"9;t: b",
            long.as_bytes(),
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..300 {
            let mut output = Vec::new();
            for _ in 0..60 {
                output.extend_from_slice(pieces[next(pieces.len())]);
            }
            let (mut runs, mut bytes) = (OscScanner::default(), OscScanner::default());
            let mut at = 0;
            while at < output.len() {
                let end = (at + 1 + next(30)).min(output.len());
                let read = &output[at..end];
                assert_eq!(runs.scan(read), scan_every_byte(&mut bytes, read));
                assert!(runs.state == bytes.state && runs.buf == bytes.buf && runs.echo == bytes.echo);
                at = end;
            }
        }
    }

    #[test]
    fn titles_and_notifications_lose_bidi_controls_and_keep_joiners() {
        let spoof = "\u{202e}cod.txt\u{2066}x\u{2069}\u{200f}\u{61c}\u{200b}\u{feff}";
        assert_eq!(sanitize_title(spoof), "cod.txtx");
        assert_eq!(
            parse_osc(format!("777;notify;{spoof};{spoof}").as_bytes()),
            Some(Osc::Notify { title: "cod.txtx".into(), body: "cod.txtx".into() })
        );
        // A family emoji (ZWJ) and Persian "mikhaham" (ZWNJ) stay whole.
        let joined =
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} \u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}";
        assert_eq!(sanitize_title(joined), joined);
        assert_eq!(clean(joined), joined);
    }
}
