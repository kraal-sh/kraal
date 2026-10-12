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
    s.chars().filter(|c| !c.is_control()).take(120).collect()
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

impl OscScanner {
    /// The sequences that end in `bytes`, each with the offset just past its end.
    pub(super) fn scan(&mut self, bytes: &[u8]) -> Vec<(usize, Osc)> {
        let mut out = Vec::new();
        for (i, &b) in bytes.iter().enumerate() {
            // Something between a notification and the next sequence.
            let between = match self.state {
                ScanState::Ground => b != 0x1b,
                ScanState::Esc => b != b']',
                _ => false,
            };
            if between {
                self.echo = None;
            }
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
                    out.extend(self.finish().map(|o| (i + 1, o)));
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
                    out.extend(self.finish().map(|o| (i + 1, o)));
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
