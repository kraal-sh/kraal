//! Markdown laid out as styled lines: headings, lists, quotes, code, tables, links.

use std::borrow::Cow;
use std::collections::HashSet;
use std::mem;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::spans::{expand_tabs, hard_wrap, single_line, spans_width, truncate_spans, truncate_str, wrap_spans};
use super::{
    BULLETS, CODE_BLOCK, CODE_LABEL, IMAGE_ALT, INLINE_CODE, LINK, MIN_COLUMN_WIDTH, QUOTE_TEXT, TASK_DONE, bullet,
    dim, link_url, quote_bar, rule, table_border, task_todo,
};
use crate::text::{sanitize, visible_char};

/// Renders Markdown into lines wrapped to `width` columns.
pub(super) fn markdown_lines(source: &str, width: usize) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events: Vec<Event<'_>> = Parser::new_ext(source, options).collect();
    MarkdownRenderer::new(width).render(&events)
}

/// A block container (quote or list item) contributing a prefix to each line.
pub(super) struct Container {
    /// Prefix of the container's first line (e.g. the bullet).
    first: Vec<Span<'static>>,
    /// Prefix of the following lines (the hanging indent).
    rest: Vec<Span<'static>>,
    /// Whether `first` has yet to be emitted.
    fresh: bool,
}

pub(super) struct ListState {
    /// Next number for ordered lists.
    next: Option<u64>,
    number_width: usize,
}

pub(super) struct CodeBlock {
    lang: String,
    text: String,
}

#[derive(Default)]
pub(super) struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    header_rows: usize,
}

pub(super) struct MarkdownRenderer {
    width: usize,
    lines: Vec<Line<'static>>,
    containers: Vec<Container>,
    lists: Vec<ListState>,
    /// Inline content of the current paragraph, heading or table cell.
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// Open links: destination and where their text starts in `inline`.
    links: Vec<(String, usize)>,
    shown_urls: HashSet<String>,
    code: Option<CodeBlock>,
    html: Option<String>,
    table: Option<TableState>,
    need_blank: bool,
}

impl MarkdownRenderer {
    fn new(width: usize) -> Self {
        MarkdownRenderer {
            width: width.max(1),
            lines: Vec::new(),
            containers: Vec::new(),
            lists: Vec::new(),
            inline: Vec::new(),
            styles: Vec::new(),
            links: Vec::new(),
            shown_urls: HashSet::new(),
            code: None,
            html: None,
            table: None,
            need_blank: false,
        }
    }

    fn render(mut self, events: &[Event<'_>]) -> Vec<Line<'static>> {
        for (index, event) in events.iter().enumerate() {
            self.event(event, &events[index + 1..]);
        }
        self.flush_inline();
        self.lines
    }

    fn event(&mut self, event: &Event<'_>, rest: &[Event<'_>]) {
        match event {
            Event::Start(tag) => self.start(tag, rest),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some(code) = &mut self.code {
                    code.text.push_str(text);
                } else if let Some(html) = &mut self.html {
                    html.push_str(text);
                } else {
                    self.push_text(text, self.style());
                }
            }
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push_text(text, self.style().patch(INLINE_CODE));
            }
            Event::Html(text) => match &mut self.html {
                Some(html) => html.push_str(text),
                None => self.push_inline_html(text),
            },
            Event::InlineHtml(text) => self.push_inline_html(text),
            Event::FootnoteReference(name) => self.push_text(&format!("[^{name}]"), dim()),
            Event::SoftBreak => self.push_text(" ", self.style()),
            Event::HardBreak => self.inline.push(Span::raw("\n")),
            Event::Rule => {
                self.start_block();
                self.emit(vec![Span::styled("─".repeat(self.avail()), rule())]);
                self.need_blank = true;
            }
            Event::TaskListMarker(done) => self.task_marker(*done),
        }
    }

    fn start(&mut self, tag: &Tag<'_>, rest: &[Event<'_>]) {
        match tag {
            Tag::Paragraph
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => self.start_block(),
            Tag::Heading { level, .. } => {
                self.start_block();
                self.styles.push(heading_style(*level));
            }
            Tag::BlockQuote(_) => {
                self.start_block();
                let bar = vec![Span::styled("│ ", quote_bar())];
                self.containers.push(Container { first: bar.clone(), rest: bar, fresh: false });
                self.styles.push(QUOTE_TEXT);
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split([' ', '\t', ',', '{']).next().unwrap_or_default().to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeBlock { lang, text: String::new() });
            }
            Tag::HtmlBlock => {
                // The blank-line decision is deferred: comments are dropped.
                self.flush_inline();
                self.html = Some(String::new());
            }
            Tag::List(start) => {
                self.start_block();
                let number_width = start.map_or(0, |first| {
                    let last = first.saturating_add(count_items(rest).saturating_sub(1) as u64);
                    last.to_string().len()
                });
                self.lists.push(ListState { next: *start, number_width });
            }
            Tag::Item => {
                self.start_block();
                let marker = self.next_marker();
                let indent = " ".repeat(marker.width());
                self.containers.push(Container {
                    first: vec![Span::styled(marker, bullet())],
                    rest: vec![Span::raw(indent)],
                    fresh: true,
                });
            }
            Tag::Table(alignments) => {
                self.start_block();
                self.table = Some(TableState { alignments: alignments.clone(), ..TableState::default() });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.row.clear();
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self.styles.push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.styles.push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Superscript | Tag::Subscript => self.styles.push(Style::new()),
            Tag::Link { dest_url, .. } => {
                self.styles.push(LINK);
                self.links.push((dest_url.to_string(), self.inline.len()));
            }
            Tag::Image { .. } => {
                self.styles.push(IMAGE_ALT);
                self.inline.push(Span::styled("[image: ", self.style()));
            }
        }
    }

    fn end(&mut self, tag: &TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_inline();
                self.need_blank = true;
            }
            TagEnd::Heading(level) => {
                self.styles.pop();
                self.finish_heading(*level);
                self.need_blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.styles.pop();
                self.containers.pop();
                self.need_blank = true;
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.emit_code(&code);
                }
                self.need_blank = true;
            }
            TagEnd::HtmlBlock => {
                if let Some(html) = self.html.take() {
                    self.emit_html(&html);
                }
            }
            TagEnd::List(_) => {
                self.flush_inline();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.need_blank = true;
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                // An empty item still shows its marker.
                if self.containers.last().is_some_and(|c| c.fresh) {
                    self.emit(Vec::new());
                }
                self.containers.pop();
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
                self.need_blank = true;
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    let row = mem::take(&mut table.row);
                    table.rows.push(row);
                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = mem::take(&mut self.inline);
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Superscript | TagEnd::Subscript => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, start)) = self.links.pop() {
                    self.link_suffix(url, start);
                }
            }
            TagEnd::Image => {
                self.inline.push(Span::styled("]", self.style()));
                self.styles.pop();
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {
                self.flush_inline();
                self.need_blank = true;
            }
        }
    }

    /// The current inline style: all open styles patched together.
    fn style(&self) -> Style {
        self.styles.iter().fold(Style::new(), |acc, style| acc.patch(*style))
    }

    fn push_text(&mut self, text: &str, style: Style) {
        let clean: String = text
            .chars()
            .map(|c| match c {
                '\n' | '\r' | '\t' => ' ',
                c => visible_char(c),
            })
            .collect();
        self.inline.push(Span::styled(clean, style));
    }

    fn push_inline_html(&mut self, html: &str) {
        let spans = html_spans(html, self.style());
        self.inline.extend(spans);
    }

    fn task_marker(&mut self, done: bool) {
        let (text, style) = if done { ("[x] ", TASK_DONE) } else { ("[ ] ", task_todo()) };
        let ordered = self.lists.last().is_some_and(|list| list.next.is_some());
        match self.containers.last_mut() {
            Some(item) if item.fresh => {
                let marker = Span::styled(text, style);
                if ordered {
                    item.first.push(marker);
                    item.rest.push(Span::raw(" ".repeat(text.len())));
                } else {
                    // Checkboxes replace bullets.
                    item.first = vec![marker];
                    item.rest = vec![Span::raw(" ".repeat(text.len()))];
                }
            }
            _ => self.inline.push(Span::styled(text, style)),
        }
    }

    /// Appends a link's URL after its text, unless redundant or already shown.
    fn link_suffix(&mut self, url: String, start: usize) {
        if url.is_empty() || url.starts_with('#') {
            return;
        }
        let text: String =
            self.inline[start.min(self.inline.len())..].iter().map(|span| span.content.as_ref()).collect();
        let text = text.trim();
        if text == url || Some(text) == url.strip_prefix("mailto:") || !self.shown_urls.insert(url.clone()) {
            return;
        }
        self.push_text(&format!(" ({url})"), link_url());
    }

    fn next_marker(&mut self) -> String {
        let depth = self.lists.len().saturating_sub(1);
        match self.lists.last_mut() {
            Some(ListState { next: Some(number), number_width }) => {
                let marker = format!("{number:>width$}. ", width = *number_width);
                *number = number.saturating_add(1);
                marker
            }
            _ => format!("{} ", BULLETS[depth % BULLETS.len()]),
        }
    }

    /// Width available for content after the container prefixes.
    fn avail(&self) -> usize {
        let prefix: usize = self.containers.iter().map(|c| spans_width(&c.rest)).sum();
        self.width.saturating_sub(prefix).max(1)
    }

    /// Flushes pending inline text and emits a separating blank line if needed.
    fn start_block(&mut self) {
        self.flush_inline();
        if self.need_blank && !self.lines.is_empty() {
            self.emit_blank();
        }
        self.need_blank = false;
    }

    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let spans = mem::take(&mut self.inline);
        for line in wrap_spans(&spans, self.avail()) {
            self.emit(line);
        }
    }

    /// Emits one line of content behind the container prefixes.
    fn emit(&mut self, content: Vec<Span<'static>>) {
        let mut spans = Vec::with_capacity(self.containers.len() + content.len());
        for container in &mut self.containers {
            let prefix = if container.fresh { &container.first } else { &container.rest };
            spans.extend(prefix.iter().cloned());
            container.fresh = false;
        }
        spans.extend(content);
        self.lines.push(Line::from(spans));
    }

    fn emit_blank(&mut self) {
        let mut spans: Vec<Span<'static>> = self.containers.iter().flat_map(|c| c.rest.iter().cloned()).collect();
        while let Some(last) = spans.last_mut() {
            let trimmed = last.content.trim_end();
            if trimmed.is_empty() {
                spans.pop();
            } else {
                if trimmed.len() != last.content.len() {
                    last.content = Cow::Owned(trimmed.to_owned());
                }
                break;
            }
        }
        self.lines.push(Line::from(spans));
    }

    fn finish_heading(&mut self, level: HeadingLevel) {
        let spans = mem::take(&mut self.inline);
        let wrapped = wrap_spans(&spans, self.avail());
        let text_width = wrapped.iter().map(|line| spans_width(line)).max().unwrap_or(0);
        for line in wrapped {
            self.emit(line);
        }
        let style = heading_style(level).remove_modifier(Modifier::BOLD);
        match level {
            HeadingLevel::H1 => self.emit(vec![Span::styled("━".repeat(self.avail()), style)]),
            HeadingLevel::H2 if text_width > 0 => self.emit(vec![Span::styled("─".repeat(text_width), style)]),
            _ => {}
        }
    }

    fn emit_code(&mut self, code: &CodeBlock) {
        let avail = self.avail();
        let indent = if avail >= 8 { 2 } else { 0 };
        let box_width = avail - indent;
        let inner = box_width.saturating_sub(1).max(1);
        let boxed = |text: String, style: Style| {
            let pad = box_width.saturating_sub(1 + text.width());
            vec![Span::raw(" ".repeat(indent)), Span::styled(format!(" {text}{}", " ".repeat(pad)), style)]
        };
        if !code.lang.is_empty() {
            self.emit(boxed(truncate_str(&sanitize(&code.lang), inner), CODE_LABEL));
        }
        let text = code.text.strip_suffix('\n').unwrap_or(&code.text);
        for raw in text.split('\n') {
            let line = expand_tabs(raw.strip_suffix('\r').unwrap_or(raw));
            for chunk in hard_wrap(&line, inner) {
                self.emit(boxed(chunk, CODE_BLOCK));
            }
        }
    }

    /// Shows the text of an HTML block, one line per source line; blocks
    /// made only of tags or comments disappear.
    fn emit_html(&mut self, html: &str) {
        let spans = html_spans(html, self.style());
        let lines: Vec<_> = wrap_spans(&spans, self.avail()).into_iter().filter(|line| spans_width(line) > 0).collect();
        if lines.is_empty() {
            return;
        }
        self.start_block();
        for line in lines {
            self.emit(line);
        }
        self.need_blank = true;
    }

    fn emit_table(&mut self, table: TableState) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0).max(table.alignments.len());
        if columns == 0 {
            return;
        }
        let rows: Vec<Vec<Vec<Span<'static>>>> =
            table.rows.into_iter().map(|row| row.into_iter().map(|cell| single_line(&cell)).collect()).collect();
        let mut widths = vec![1; columns];
        for row in &rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(spans_width(cell));
            }
        }
        let separators = 3 * (columns - 1);
        fit_columns(&mut widths, self.avail().saturating_sub(separators));
        for (index, row) in rows.iter().enumerate() {
            let header = index < table.header_rows;
            let mut spans = Vec::new();
            for (column, &width) in widths.iter().enumerate() {
                if column > 0 {
                    spans.push(Span::styled(" │ ", table_border()));
                }
                let cell = row.get(column).map_or(&[][..], Vec::as_slice);
                let mut cell = truncate_spans(cell, width);
                if header {
                    cell = cell.into_iter().map(|s| s.patch_style(Modifier::BOLD)).collect();
                }
                let gap = width.saturating_sub(spans_width(&cell));
                let (left, right) = match table.alignments.get(column) {
                    Some(Alignment::Right) => (gap, 0),
                    Some(Alignment::Center) => (gap / 2, gap - gap / 2),
                    _ => (0, gap),
                };
                if left > 0 {
                    spans.push(Span::raw(" ".repeat(left)));
                }
                spans.extend(cell);
                if right > 0 {
                    spans.push(Span::raw(" ".repeat(right)));
                }
            }
            self.emit(spans);
            if header && index + 1 == table.header_rows {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                self.emit(vec![Span::styled(rule.join("─┼─"), table_border())]);
            }
        }
    }
}

/// Shrink the widest columns until they fit in `room`, none below [`MIN_COLUMN_WIDTH`] (rows are
/// then simply truncated): every column gets at most a common cap, the largest that fits, and
/// what is left of the room goes one column each to the widest. Linear in the columns, whatever
/// their widths.
pub(super) fn fit_columns(widths: &mut [usize], room: usize) {
    let total = |cap: usize| widths.iter().map(|w| (*w).min(cap)).sum::<usize>();
    if total(usize::MAX) <= room {
        return;
    }
    let (mut low, mut high) = (MIN_COLUMN_WIDTH, widths.iter().copied().max().unwrap_or(0));
    if total(low) > room {
        for w in widths.iter_mut() {
            *w = (*w).min(MIN_COLUMN_WIDTH);
        }
        return;
    }
    // The largest cap whose total fits.
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if total(mid) <= room { low = mid } else { high = mid - 1 }
    }
    let mut spare = room - total(low);
    for w in widths.iter_mut() {
        let wider = *w > low && spare > 0;
        *w = (*w).min(low) + usize::from(wider);
        spare -= usize::from(wider);
    }
}

pub(super) fn heading_style(level: HeadingLevel) -> Style {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 => bold.fg(Color::LightMagenta),
        HeadingLevel::H2 => bold.fg(Color::LightCyan),
        HeadingLevel::H3 => bold.fg(Color::LightYellow),
        HeadingLevel::H4 => bold.fg(Color::LightGreen),
        HeadingLevel::H5 | HeadingLevel::H6 => bold.add_modifier(Modifier::ITALIC),
    }
}

/// Converts raw HTML to styled text: tags and comments are dropped, `<br>`
/// becomes a line break, `<img>` its alt text, and common entities are decoded.
pub(super) fn html_spans(html: &str, style: Style) -> Vec<Span<'static>> {
    fn flush(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
        if !text.is_empty() {
            let clean: String = decode_entities(text)
                .chars()
                .map(|c| match c {
                    '\n' => c,
                    '\t' | '\r' => ' ',
                    c => visible_char(c),
                })
                .collect();
            spans.push(Span::styled(clean, style));
            text.clear();
        }
    }
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        let (before, tail) = rest.split_at(start);
        text.push_str(before);
        let end = if tail.starts_with("<!--") {
            Some(tail.find("-->").map_or(tail.len(), |i| i + 3))
        } else if tail[1..].starts_with(|c: char| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?')) {
            tail.find('>').map(|i| i + 1)
        } else {
            None
        };
        let Some(end) = end else {
            // A lone `<` is text.
            text.push('<');
            rest = &tail[1..];
            continue;
        };
        let tag = &tail[..end];
        let name = tag
            .trim_start_matches(['<', '/'])
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match name.as_str() {
            "br" => text.push('\n'),
            "img" => {
                flush(&mut spans, &mut text, style);
                let alt = html_attr(tag, "alt").map(decode_entities).unwrap_or_default();
                let label = if alt.trim().is_empty() {
                    "[image]".to_owned()
                } else {
                    format!("[image: {}]", sanitize(alt.trim()))
                };
                spans.push(Span::styled(label, style.patch(IMAGE_ALT)));
            }
            _ => {}
        }
        rest = &tail[end..];
    }
    text.push_str(rest);
    flush(&mut spans, &mut text, style);
    spans
}

/// The value of attribute `name` in an HTML start tag, if present.
pub(super) fn html_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    // ASCII lowercasing keeps byte offsets, so indices map back to `tag`.
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let at = from + found;
        from = at + needle.len();
        if !lower[..at].ends_with(char::is_whitespace) {
            continue;
        }
        let value = &tag[from..];
        return match value.chars().next() {
            Some(quote @ ('"' | '\'')) => value[1..].split(quote).next(),
            _ => value.split(|c: char| c.is_whitespace() || c == '>').next(),
        };
    }
    None
}

/// Decodes the handful of HTML entities common in READMEs.
pub(super) fn decode_entities(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    const ENTITIES: [(&str, &str); 7] = [
        ("&nbsp;", "\u{a0}"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&amp;", "&"),
    ];
    let mut out = text.to_owned();
    for (entity, replacement) in ENTITIES {
        out = out.replace(entity, replacement);
    }
    Cow::Owned(out)
}

/// Counts the items of the list whose events follow its `Start(List)`.
pub(super) fn count_items(rest: &[Event<'_>]) -> usize {
    let mut depth = 0usize;
    let mut count = 0;
    for event in rest {
        match event {
            Event::Start(Tag::List(_)) => depth += 1,
            Event::End(TagEnd::List(_)) if depth == 0 => break,
            Event::End(TagEnd::List(_)) => depth -= 1,
            Event::Start(Tag::Item) if depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}
