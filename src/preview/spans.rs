//! Styled text: widths, truncation and wrapping of spans.

use std::mem;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::TAB_WIDTH;
use crate::text::visible_char;

/// Expands tabs to 4-column stops and makes control characters visible.
pub(super) fn expand_tabs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut column = 0;
    for c in line.chars() {
        if c == '\t' {
            let spaces = TAB_WIDTH - column % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            let c = visible_char(c);
            out.push(c);
            column += c.width().unwrap_or(0);
        }
    }
    out
}

pub(super) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

pub(super) fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|span| span.content.as_ref()).collect()
}

/// Cuts `text` to at most `width` columns.
pub(super) fn truncate_str(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

/// Splits `text` into chunks of at most `width` columns (at least one char each).
pub(super) fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let mut chunks = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && used > 0 {
            chunks.push(String::new());
            used = 0;
        }
        if let Some(chunk) = chunks.last_mut() {
            chunk.push(c);
        }
        used += w;
    }
    chunks
}

/// Joins hard breaks of a table cell into spaces.
pub(super) fn single_line(spans: &[Span<'static>]) -> Vec<Span<'static>> {
    spans
        .iter()
        .map(|span| {
            if span.content.contains('\n') {
                Span::styled(span.content.replace('\n', " "), span.style)
            } else {
                span.clone()
            }
        })
        .collect()
}

/// Cuts styled text to `width` columns, ending with `…` when shortened.
pub(super) fn truncate_spans(spans: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    if spans_width(spans) <= width {
        return spans.to_vec();
    }
    if width == 0 {
        return Vec::new();
    }
    let budget = width - 1;
    let mut used = 0;
    let mut out = Vec::new();
    let mut style = Style::new();
    'spans: for span in spans {
        style = span.style;
        let mut text = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > budget {
                if !text.is_empty() {
                    out.push(Span::styled(text, style));
                }
                break 'spans;
            }
            text.push(c);
            used += w;
        }
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
    }
    out.push(Span::styled("…", style));
    out
}

/// Greedy word wrapping of styled text; `\n` forces a break.
pub(super) fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut wrapper = Wrapper::new(width);
    let mut word: Vec<Span<'static>> = Vec::new();
    for span in spans {
        let mut piece = String::new();
        for c in span.content.chars() {
            if c == ' ' || c == '\n' {
                if !piece.is_empty() {
                    word.push(Span::styled(mem::take(&mut piece), span.style));
                }
                wrapper.word(mem::take(&mut word));
                if c == '\n' {
                    wrapper.break_line();
                } else {
                    wrapper.space(span.style);
                }
            } else {
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            word.push(Span::styled(piece, span.style));
        }
    }
    wrapper.word(word);
    wrapper.finish()
}

pub(super) struct Wrapper {
    pub(super) width: usize,
    pub(super) lines: Vec<Vec<Span<'static>>>,
    pub(super) line: Vec<Span<'static>>,
    pub(super) line_width: usize,
    /// A space seen since the last word, emitted only if the next word fits.
    pub(super) pending_space: Option<Style>,
}

impl Wrapper {
    fn new(width: usize) -> Self {
        Wrapper { width: width.max(1), lines: Vec::new(), line: Vec::new(), line_width: 0, pending_space: None }
    }

    fn space(&mut self, style: Style) {
        if self.line_width > 0 && self.pending_space.is_none() {
            self.pending_space = Some(style);
        }
    }

    fn break_line(&mut self) {
        self.lines.push(mem::take(&mut self.line));
        self.line_width = 0;
        self.pending_space = None;
    }

    fn word(&mut self, word: Vec<Span<'static>>) {
        if word.is_empty() {
            return;
        }
        let width = spans_width(&word);
        let space = usize::from(self.pending_space.is_some());
        if self.line_width + space + width <= self.width {
            if let Some(style) = self.pending_space.take() {
                self.push(" ", style);
            }
            for span in &word {
                self.push(&span.content, span.style);
            }
        } else if width <= self.width {
            self.break_line();
            for span in &word {
                self.push(&span.content, span.style);
            }
        } else {
            // Too long for any line: split it, starting on the current line.
            if let Some(style) = self.pending_space.take() {
                if self.line_width + 1 < self.width {
                    self.push(" ", style);
                } else {
                    self.break_line();
                }
            }
            for span in &word {
                for c in span.content.chars() {
                    if self.line_width + c.width().unwrap_or(0) > self.width && self.line_width > 0 {
                        self.break_line();
                    }
                    self.push(c.encode_utf8(&mut [0; 4]), span.style);
                }
            }
        }
    }

    fn push(&mut self, text: &str, style: Style) {
        self.line_width += text.width();
        match self.line.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(text),
            _ => self.line.push(Span::styled(text.to_owned(), style)),
        }
    }

    fn finish(mut self) -> Vec<Vec<Span<'static>>> {
        if !self.line.is_empty() {
            self.lines.push(self.line);
        }
        self.lines
    }
}
