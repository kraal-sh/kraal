//! Paste buffers from the command line: list, show, set, paste, delete.

use std::io::{IsTerminal, Read, Write};

use anyhow::{Context, Result, bail};

use super::args::BufferCmd;
use super::{Session, pane_ref};
use crate::protocol::{Request, Response};

/// The most `kraal buffer set` reads: a message to the server carries 64 MiB, escapes included.
const MAX_SET: u64 = 32 << 20;

pub(super) fn run(s: &Session, action: BufferCmd) -> Result<()> {
    match action {
        BufferCmd::List { json } => list(s, json),
        BufferCmd::Show { buffer } => {
            if let Response::Text(text) = s.request(Request::BufferShow { buffer })? {
                let mut out = std::io::stdout().lock();
                out.write_all(text.as_bytes())?;
                out.flush()?;
            }
            Ok(())
        }
        BufferCmd::Set { name, from_file } => {
            let bytes = match from_file {
                Some(path) => {
                    crate::paths::read_regular(&path, MAX_SET).with_context(|| format!("reading {}", path.display()))?
                }
                None => {
                    let stdin = std::io::stdin();
                    if stdin.is_terminal() {
                        bail!("give the text on the standard input (`echo hi | kraal buffer set`), or --from-file");
                    }
                    let mut bytes = Vec::new();
                    stdin.lock().take(MAX_SET + 1).read_to_end(&mut bytes)?;
                    if bytes.len() as u64 > MAX_SET {
                        bail!("more than {} MiB: too long for a buffer", MAX_SET >> 20);
                    }
                    bytes
                }
            };
            let text = String::from_utf8(bytes).context("a buffer holds text: this is not UTF-8")?;
            s.request(Request::BufferSet { name, text })?;
            Ok(())
        }
        BufferCmd::Paste { buffer, pane, production } => {
            s.request(Request::BufferPaste { buffer, pane: pane_ref(s, pane)?, production })?;
            Ok(())
        }
        BufferCmd::Delete { buffer, all } => {
            s.request(Request::BufferDelete { buffer, all })?;
            Ok(())
        }
    }
}

fn list(s: &Session, json: bool) -> Result<()> {
    let Response::Buffers(list) = s.request(Request::BufferList)? else { bail!("unexpected answer") };
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    let now = crate::accounts::now();
    let width = list.iter().map(|b| crate::text::columns(&b.buffer)).max().unwrap_or(0);
    for b in &list {
        let size = crate::text::human_size(b.bytes as u64);
        let age = crate::buffers::age(now - b.at);
        let pad = " ".repeat(width.saturating_sub(crate::text::columns(&b.buffer)));
        println!("{}{pad}  {size:>9}  {age:>4}  {}", b.buffer, b.preview);
    }
    Ok(())
}
