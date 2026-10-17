use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use crossbeam_channel::{Receiver, bounded};

use super::connection::send;
use super::{Active, Controller, Dropped, Prompt};
use crate::attachments::Source;
use crate::input::InputParser;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::theme::NoticeKind;
use crate::{attachments, protocol};

impl Dropped {
    pub(super) fn validate(&self) -> Result<u64> {
        if self.image {
            return Ok(0);
        }
        let mut specs = Vec::new();
        for path in &self.paths {
            let meta = std::fs::symlink_metadata(path)?;
            ensure!(
                meta.is_file(),
                "Only regular files can be sent. Use kraal cp for directories, links and special files."
            );
            specs.push(attachments::FileSpec {
                name: path.file_name().and_then(|s| s.to_str()).unwrap_or("").into(),
                size: meta.len(),
            });
        }
        self.limits.check(&specs)
    }
}

pub(super) fn explicit_paths(text: &str) -> Result<Vec<std::path::PathBuf>> {
    dropped_paths(text).context("enter existing absolute paths, quoting spaces")
}

impl Controller {
    pub(super) fn start(self: &Arc<Self>, prompt: Prompt) {
        let mut state = self.state.lock().unwrap();
        let (ack, replies) = bounded(2);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (finished, completion) = bounded(1);
        state.workers.retain(|worker| matches!(worker.try_recv(), Err(crossbeam_channel::TryRecvError::Empty)));
        state.workers.push(completion);
        if state.dropped.is_none() {
            state.dropped = Some(Dropped {
                image: prompt.image,
                token: prompt.token.clone(),
                original: prompt.text.as_bytes().to_vec(),
                paths: if prompt.image { Vec::new() } else { explicit_paths(&prompt.text).unwrap_or_default() },
                target: prompt.target.clone(),
                limits: prompt.limits.clone(),
                ready: true,
                ..Default::default()
            });
        }
        state.success_until = None;
        state.active = Some(Active {
            token: prompt.token.clone(),
            cancelled: cancelled.clone(),
            ack,
            started: Instant::now(),
            last_draw: None,
            label: String::new(),
            progress: None,
            failure: None,
        });
        state.status = None;
        state.displayed = None;
        state.parser = InputParser::new();
        drop(state);
        self.start_progress(prompt.token.clone());
        let this = self.clone();
        let token = prompt.token.clone();
        let action = if prompt.image { "Attach clipboard image" } else { "Attach files" };
        if let Err(error) = crate::threads::named("attachments").spawn(move || {
            let _finished = finished;
            if let Err(error) = this.upload(&prompt, &replies, &cancelled)
                && !cancelled.load(Ordering::SeqCst)
            {
                this.upload_failed(prompt.token, &cancelled, &error, prompt.image);
            }
        }) {
            let _ = send(&self.writer, &ClientMsg::AttachmentCancel { token: token.clone() });
            self.message(ServerMsg::AttachmentResult {
                kind: NoticeKind::Error,
                pasted: false,
                token,
                message: format!("Cannot start attachment: {error}. Use {action} to retry."),
            });
        }
    }

    pub(super) fn upload_failed(
        self: &Arc<Self>,
        token: String,
        cancelled: &AtomicBool,
        error: &anyhow::Error,
        image: bool,
    ) {
        let message = if image {
            format!("Image not sent: {error:#}. Choose Retry to try again.")
        } else {
            format!("Files not sent: {error:#}. Choose Retry to try again.")
        };
        let dropped = {
            let mut state = self.state.lock().unwrap();
            if let Some(active) = state.active.as_mut().filter(|a| a.token == token) {
                active.failure = Some(message.clone());
            }
            if let Some(dropped) = state.dropped.as_mut().filter(|d| d.token == token) {
                dropped.error = Some(format!("{error:#}"));
                true
            } else {
                false
            }
        };
        {
            let mut writer = self.writer.lock().unwrap();
            if !cancelled.load(Ordering::SeqCst)
                && let Some(writer) = writer.as_mut()
            {
                let _ = protocol::write_json(writer, &ClientMsg::AttachmentCancel { token: token.clone() });
            }
        }
        if !dropped {
            self.message(ServerMsg::AttachmentResult { kind: NoticeKind::Error, pasted: false, token, message });
        }
    }

    pub(super) fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        let token = state.prompt.take().map(|p| p.token).or_else(|| {
            state.active.take().map(|a| {
                a.cancelled.store(true, Ordering::SeqCst);
                a.token
            })
        });
        let dismiss = state.result_token.take().or_else(|| state.uncertain.take());
        state.status_kind = NoticeKind::Notice;
        state.status = token.as_ref().map(|_| "Cancelling attachment. Waiting for its result.".into());
        state.uncertain = token.clone();
        drop(state);
        if let Some(token) = token {
            let _ = send(&self.writer, &ClientMsg::AttachmentCancel { token });
        } else if let Some(token) = dismiss {
            let _ = send(&self.writer, &ClientMsg::AttachmentDismiss { token });
        }
        self.redraw();
    }

    pub(super) fn progress(&self, token: &str, received: u64, total: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(active) = &mut state.active
            && active.token == token
        {
            active.progress = Some((received, total));
        }
    }

    pub(super) fn upload(&self, prompt: &Prompt, replies: &Receiver<u64>, cancelled: &AtomicBool) -> Result<()> {
        let started = Instant::now();
        let image = prompt.image.then(|| super::super::clipboard_image::read(cancelled)).transpose()?;
        let paths = if let Some(image) = &image { vec![image.path.clone()] } else { explicit_paths(&prompt.text)? };
        ensure!(paths.len() <= prompt.limits.files, "too many files; use kraal cp");
        let mut sources = paths.into_iter().map(Source::open).collect::<Result<Vec<_>>>()?;
        let specs: Vec<_> = sources.iter().map(|s| s.spec.clone()).collect();
        let token = &prompt.token;
        let write = |msg: ClientMsg| -> Result<()> {
            let mut writer = self.writer.lock().unwrap();
            ensure!(!cancelled.load(Ordering::SeqCst), "attachment cancelled");
            ensure!(started.elapsed() < attachments::TIMEOUT, "attachment timed out");
            protocol::write_json(writer.as_mut().context("connection closed")?, &msg)?;
            Ok(())
        };
        let wait = |expected| -> Result<()> {
            loop {
                ensure!(!cancelled.load(Ordering::SeqCst), "attachment cancelled");
                ensure!(started.elapsed() < attachments::TIMEOUT, "attachment timed out");
                match replies.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(received) => {
                        ensure!(received == expected, "unexpected attachment acknowledgement");
                        return Ok(());
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(_) => bail!("connection closed"),
                }
            }
        };
        if !self.remote && !prompt.image {
            let paths = sources
                .iter()
                .map(|s| s.path.to_str().context("path is not UTF-8").map(String::from))
                .collect::<Result<_>>()?;
            write(ClientMsg::AttachmentLocal { token: token.clone(), paths })?;
            return Ok(());
        }
        let total = prompt.limits.check(&specs)?;
        {
            let mut state = self.state.lock().unwrap();
            if let Some(active) = state.active.as_mut().filter(|active| active.token == *token) {
                active.label = if specs.len() == 1 { specs[0].name.clone() } else { format!("{} files", specs.len()) };
            }
        }
        write(ClientMsg::AttachmentStart { token: token.clone(), files: specs })?;
        self.progress(token, 0, total);
        wait(0)?;
        let mut received = 0;
        for (file, source) in sources.iter_mut().enumerate() {
            let mut offset = 0;
            while offset < source.spec.size {
                ensure!(!cancelled.load(Ordering::SeqCst), "attachment cancelled");
                let bytes = source.chunk(offset)?;
                let size = bytes.len() as u64;
                write(ClientMsg::AttachmentChunk {
                    token: token.clone(),
                    file,
                    offset,
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                })?;
                offset += size;
                received += size;
                wait(received)?;
                self.progress(token, received, total);
            }
            source.chunk(offset)?;
        }
        write(ClientMsg::AttachmentFinish { token: token.clone() })?;
        Ok(())
    }
}

/// Recognize complete local path lists without expanding shell expressions.
pub(super) fn dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    if text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return None;
    }
    let text = text.trim();
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                let next = chars.next()?;
                if !matches!(next, '"' | '\\' | '$' | '`') {
                    cur.push('\\');
                }
                cur.push(next);
            }
            (Some('"') | None, '$' | '`') => return None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                cur.push(chars.next()?);
                in_word = true;
            }
            (None, '*' | '?' | '[' | ']' | ';' | '|' | '&' | '<' | '>' | '(' | ')') => return None,
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(cur);
    }
    let parsed =
        words.into_iter().map(|word| local_path(&word)).collect::<Option<Vec<_>>>().filter(|paths| !paths.is_empty());
    // Some terminals leave spaces unquoted. Never choose between a whole name and a list.
    let raw = (!text.contains(['\'', '"', '\\', '\n', '\r', '\t']))
        .then(|| local_path(text))
        .flatten()
        .map(|path| vec![path]);
    if parsed.is_some() {
        let words: Vec<_> = text.split_whitespace().collect();
        for (i, word) in words.iter().enumerate() {
            let start = word.as_ptr() as usize - text.as_ptr() as usize;
            let Some(next) = words.get(i + 1) else { break };
            let next_start = next.as_ptr() as usize - text.as_ptr() as usize;
            // Joining absolute words would need a directory whose name includes the separator.
            if !Path::new(&text[start..next_start]).is_dir() {
                continue;
            }
            for last in &words[i + 1..] {
                let last_end = last.as_ptr() as usize - text.as_ptr() as usize + last.len();
                if last_end - start > 4096 {
                    break;
                }
                let joined = &text[start..last_end];
                if !joined.contains(['\'', '"', '\\', '\n', '\r', '\t']) && local_path(joined).is_some() {
                    return None;
                }
            }
        }
    }
    match (parsed, raw) {
        (Some(a), Some(b)) if a != b => None,
        (Some(paths), _) | (_, Some(paths)) => Some(paths),
        _ => None,
    }
}

fn local_path(word: &str) -> Option<PathBuf> {
    let decoded;
    let word = if let Some(uri) = word.strip_prefix("file://") {
        if !uri.starts_with('/') || uri.contains(['?', '#']) {
            return None;
        }
        let mut bytes = Vec::new();
        let mut input = uri.bytes();
        while let Some(byte) = input.next() {
            bytes.push(if byte == b'%' {
                let hi = char::from(input.next()?).to_digit(16)?;
                let lo = char::from(input.next()?).to_digit(16)?;
                (hi * 16 + lo) as u8
            } else {
                byte
            });
        }
        decoded = String::from_utf8(bytes).ok()?;
        decoded.as_str()
    } else {
        word
    };
    if word.chars().any(char::is_control) {
        return None;
    }
    let path = PathBuf::from(word);
    (path.is_absolute() && std::fs::symlink_metadata(&path).is_ok()).then_some(path)
}
