//! Keep complete bracketed pastes local until their meaning is known.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

const START: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";
pub const LIMIT: usize = 64 * 1024;

pub enum Part {
    Start,
    Bytes(Vec<u8>),
    Paste(Vec<u8>),
}

#[derive(Default)]
pub struct Splitter {
    marker: Vec<u8>,
    paste: Option<Vec<u8>>,
    streaming: bool,
}

impl Splitter {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Part> {
        let mut parts = Vec::new();
        let mut raw = Vec::new();
        for &byte in bytes {
            self.marker.push(byte);
            let expected = if self.paste.is_some() || self.streaming { END } else { START };
            while !expected.starts_with(&self.marker) {
                let byte = self.marker.remove(0);
                if let Some(paste) = &mut self.paste {
                    if paste.is_empty() {
                        if !raw.is_empty() {
                            parts.push(Part::Bytes(std::mem::take(&mut raw)));
                        }
                        parts.push(Part::Start);
                    }
                    paste.push(byte);
                    if paste.len() > LIMIT {
                        raw.extend_from_slice(START);
                        raw.extend(self.paste.take().unwrap());
                        self.streaming = true;
                    }
                } else {
                    raw.push(byte);
                }
            }
            if self.marker == expected {
                self.marker.clear();
                if self.streaming {
                    raw.extend_from_slice(END);
                    self.streaming = false;
                } else if let Some(paste) = self.paste.take() {
                    if !raw.is_empty() {
                        parts.push(Part::Bytes(std::mem::take(&mut raw)));
                    }
                    parts.push(Part::Paste(paste));
                } else {
                    self.paste = Some(Vec::new());
                }
            }
        }
        if !raw.is_empty() {
            parts.push(Part::Bytes(raw));
        }
        parts
    }

    pub fn has_pending(&self) -> bool {
        self.paste.is_none() && !self.streaming && !self.marker.is_empty()
    }

    pub fn idle(&mut self) -> Vec<u8> {
        if self.paste.is_none() && !self.streaming { std::mem::take(&mut self.marker) } else { Vec::new() }
    }
}

pub fn bracket(text: &[u8]) -> Vec<u8> {
    [START, text, END].concat()
}

#[derive(Default, Serialize, Deserialize)]
struct Permissions {
    #[serde(default)]
    server: Vec<Permission>,
}

#[derive(Serialize, Deserialize)]
struct Permission {
    name: String,
    ssh: Vec<String>,
}

pub fn permission_path() -> PathBuf {
    crate::paths::config_dir().join("paste-files.toml")
}

fn permissions(path: &Path) -> Result<Permissions> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Permissions::default()),
        Err(e) => Err(e.into()),
    }
}

pub fn allowed(path: &Path, ssh: &[String]) -> bool {
    permissions(path).is_ok_and(|p| p.server.iter().any(|server| server.ssh == ssh))
}

pub fn remember(path: &Path, ssh: &[String], name: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::paths::locked(path, || {
        let mut p = permissions(path)?;
        if !p.server.iter().any(|server| server.ssh == ssh) {
            p.server.push(Permission { name: name.into(), ssh: ssh.to_vec() });
        }
        crate::paths::write_atomic(path, toml::to_string_pretty(&p)?.as_bytes())?;
        Ok(())
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flatten(parts: Vec<Part>) -> Vec<u8> {
        parts
            .into_iter()
            .flat_map(|part| match part {
                Part::Start => Vec::new(),
                Part::Bytes(bytes) => bytes,
                Part::Paste(bytes) => bracket(&bytes),
            })
            .collect()
    }

    #[test]
    fn every_marker_split_and_bytewise_input_preserve_order_and_original_bytes() {
        let input = [b"before\x1b[A".as_slice(), &bracket(b"'/a b' /c\n"), b"after", &bracket(b""), b"\xff"].concat();
        for split in 0..=input.len() {
            let mut parser = Splitter::default();
            let mut parts = parser.feed(&input[..split]);
            parts.extend(parser.feed(&input[split..]));
            assert_eq!(flatten(parts), input, "split {split}");
        }
        let mut parser = Splitter::default();
        let parts = input.iter().flat_map(|b| parser.feed(&[*b])).collect();
        assert_eq!(flatten(parts), input);
    }

    #[test]
    fn large_pastes_stream_unchanged_and_never_offer_a_valid_prefix() {
        let mut parser = Splitter::default();
        let input = bracket(&vec![b'x'; LIMIT + 1]);
        let parts: Vec<_> = input.chunks(3).flat_map(|bytes| parser.feed(bytes)).collect();
        assert!(matches!(parts.first(), Some(Part::Start)));
        assert!(parts[1..].iter().all(|part| matches!(part, Part::Bytes(_))));
        assert_eq!(flatten(parts), input);
        assert!(
            matches!(parser.feed(&bracket(b"/next")).as_slice(), [Part::Start, Part::Paste(text)] if text == b"/next")
        );
    }

    #[test]
    fn idle_escape_is_released_but_an_incomplete_paste_is_not() {
        let mut parser = Splitter::default();
        assert!(parser.feed(b"\x1b").is_empty());
        assert_eq!(parser.idle(), b"\x1b");
        assert!(matches!(parser.feed(b"\x1b[200~/some/path\x1b[20").as_slice(), [Part::Start]));
        assert!(parser.idle().is_empty());
        assert!(matches!(parser.feed(b"1~").as_slice(), [Part::Paste(text)] if text == b"/some/path"));
    }

    #[test]
    fn permission_is_destination_bound_persistent_revocable_and_fail_closed() {
        let root = std::env::temp_dir().join(format!("kraal-paste-permission-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("paste-files.toml");
        let ssh = vec!["-p".into(), "2222".into(), "alice@box".into()];
        assert!(!allowed(&path, &ssh));
        remember(&path, &ssh, "box").unwrap();
        remember(&path, &ssh, "box").unwrap();
        assert!(allowed(&path, &ssh));
        assert_eq!(permissions(&path).unwrap().server.len(), 1);
        for other in [
            vec!["bob@box".into()],
            vec!["-p".into(), "22".into(), "alice@box".into()],
            vec!["-i".into(), "other-key".into(), "alice@box".into()],
        ] {
            assert!(!allowed(&path, &other));
        }
        std::fs::remove_file(&path).unwrap();
        assert!(!allowed(&path, &ssh));
        std::fs::write(&path, "not valid toml").unwrap();
        assert!(!allowed(&path, &ssh));
        assert!(remember(&path, &ssh, "box").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not valid toml");
        std::fs::remove_dir_all(root).unwrap();
    }
}
