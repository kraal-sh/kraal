//! Hints: what can be acted on in a pane's screen (links, paths, commits, addresses, pods…),
//! each given a short label to type.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Url,
    /// A file, maybe with `:line[:col]`.
    Path,
    /// A commit or a container id.
    Hash,
    /// `host:port`.
    Address,
    Ip,
    /// A name or host from the server inventory.
    Server,
    /// A Kubernetes pod of a Deployment (`api-7d4b9c8f5-x2x9z`).
    Pod,
    Uuid,
    /// A pattern from the config, by index.
    Custom(usize),
}

impl Kind {
    /// What Shift+label does, for the top bar.
    pub fn action(self) -> &'static str {
        match self {
            Kind::Url => "open",
            Kind::Path => "edit",
            Kind::Hash => "show",
            Kind::Address => "open",
            Kind::Ip | Kind::Server => "ssh",
            Kind::Pod => "logs",
            Kind::Uuid => "type",
            Kind::Custom(_) => "run",
        }
    }
}

/// One row of a pane's screen: its text, the screen column of each character, and the OSC 8
/// hyperlinks on it (first and last character index, target).
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub text: String,
    pub cols: Vec<u16>,
    pub links: Vec<(usize, usize, String)>,
}

/// The port and the rest (`/path?query`) of a URL to the local machine (`http://localhost:5173/x`).
pub fn loopback_port(url: &str) -> Option<(u16, String)> {
    let rest = url.split_once("://")?.1;
    // The authority ends at the path, the query or the fragment.
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = authority.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if !matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1") {
        return None;
    }
    Some((port.parse().ok()?, path.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub row: u16,
    pub col: u16,
    /// Cells covered.
    pub width: u16,
    pub text: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub label: String,
    pub m: Match,
}

/// A pattern of the `[hints.patterns]` config section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternDef {
    pub pattern: String,
    /// The tool Shift+label runs, with the match in `$KRAAL_SELECTION`.
    #[serde(default)]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HintsConfig {
    /// Letters labels are made of, the easiest first.
    pub alphabet: String,
    #[serde(default)]
    pub patterns: BTreeMap<String, PatternDef>,
}

/// User patterns, compiled.
#[derive(Debug, Default)]
pub struct Custom {
    pub patterns: Vec<(String, Regex, Option<String>)>,
}

impl Custom {
    /// Compile the config's patterns; warnings for invalid ones and unknown tools.
    pub fn build(cfg: &HintsConfig, tools: &BTreeMap<String, crate::tools::ToolDef>) -> (Custom, Vec<String>) {
        let mut out = Custom::default();
        let mut warnings = Vec::new();
        for (name, p) in &cfg.patterns {
            if let Some(t) = &p.tool
                && !tools.get(t).is_some_and(|t| t.enabled)
            {
                warnings.push(format!("hints.patterns.{name}: no tool {t:?} in [tools]"));
            }
            match Regex::new(&p.pattern) {
                Ok(re) => out.patterns.push((name.clone(), re, p.tool.clone())),
                Err(e) => warnings.push(format!("hints.patterns.{name}: {e}")),
            }
        }
        (out, warnings)
    }
}

struct Builtin {
    url: Regex,
    path: Regex,
    ip: Regex,
    localhost: Regex,
    uuid: Regex,
    pod: Regex,
    hash: Regex,
}

fn builtin() -> &'static Builtin {
    static B: OnceLock<Builtin> = OnceLock::new();
    B.get_or_init(|| Builtin {
        url: Regex::new(r#"(?:https?|ftp|file)://[^\s<>"'`|]+"#).unwrap(),
        path: Regex::new(
            r"(?:~|\.{1,2})?/[\w.@+~-]+(?:/[\w.@+~-]*)*(?::\d+(?::\d+)?)?|[\w.@+-]+(?:/[\w.@+-]+)*/[\w@+-]+\.\w+(?::\d+(?::\d+)?)?|[\w@+-]+\.[A-Za-z]\w*:\d+(?::\d+)?",
        )
        .unwrap(),
        ip: Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d{1,5})?\b").unwrap(),
        localhost: Regex::new(r"\blocalhost:\d{1,5}\b|\[::1?\]:\d{1,5}\b").unwrap(),
        uuid: Regex::new(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b").unwrap(),
        pod: Regex::new(r"\b[a-z0-9](?:[a-z0-9-]*[a-z0-9])?-[a-z0-9]{8,10}-[a-z0-9]{5}\b").unwrap(),
        hash: Regex::new(r"\b[0-9a-f]{7,64}\b").unwrap(),
    })
}

/// A candidate match in one row, by byte range.
struct Candidate {
    start: usize,
    end: usize,
    kind: Kind,
    /// Lower wins when two candidates start at the same place.
    rank: u8,
}

fn trim_url(s: &str) -> &str {
    let mut s = s.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"']);
    // A closing bracket belongs to the URL only if it opened one.
    while let Some(last) = s.chars().last()
        && let Some(open) = match last {
            ')' => Some('('),
            ']' => Some('['),
            '}' => Some('{'),
            _ => None,
        }
    {
        if s.matches(open).count() >= s.matches(last).count() {
            break;
        }
        s = &s[..s.len() - 1];
        s = s.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    }
    s
}

fn valid_ip(s: &str) -> bool {
    let host = s.split(':').next().unwrap_or(s);
    let port_ok = s.split(':').nth(1).is_none_or(|p| p.parse::<u16>().is_ok_and(|p| p > 0));
    host.split('.').all(|o| o.parse::<u8>().is_ok()) && port_ok
}

/// Characters that may not touch a path on its left (it would be part of a word or a URL).
fn glued(text: &str, start: usize) -> bool {
    text[..start].chars().last().is_some_and(|c| c.is_alphanumeric() || matches!(c, '/' | ':' | '_' | '-' | '.'))
}

fn candidates(text: &str, servers: &HashSet<String>, custom: &Custom) -> Vec<Candidate> {
    let b = builtin();
    let mut out = Vec::new();
    let mut push = |start: usize, end: usize, kind: Kind, rank: u8| {
        if end > start {
            out.push(Candidate { start, end, kind, rank });
        }
    };
    for (i, (_, re, _)) in custom.patterns.iter().enumerate() {
        for m in re.find_iter(text) {
            push(m.start(), m.end(), Kind::Custom(i), 0);
        }
    }
    for m in b.url.find_iter(text) {
        let url = trim_url(m.as_str());
        push(m.start(), m.start() + url.len(), Kind::Url, 1);
    }
    for m in b.localhost.find_iter(text) {
        push(m.start(), m.end(), Kind::Address, 2);
    }
    for m in b.ip.find_iter(text) {
        if valid_ip(m.as_str()) {
            let kind = if m.as_str().contains(':') { Kind::Address } else { Kind::Ip };
            push(m.start(), m.end(), kind, 2);
        }
    }
    for m in b.uuid.find_iter(text) {
        push(m.start(), m.end(), Kind::Uuid, 3);
    }
    for m in b.path.find_iter(text) {
        let s = m.as_str();
        if !glued(text, m.start()) && s.len() > 2 && s.chars().any(|c| c.is_alphabetic()) {
            let s = s.trim_end_matches(['.', ',', ':']);
            push(m.start(), m.start() + s.len(), Kind::Path, 4);
        }
    }
    for m in b.pod.find_iter(text) {
        // The ReplicaSet hash and the pod suffix both mix letters and digits more often than not;
        // `my-long-hyphenated-name` style words do not end in 8-10 + 5 character groups often.
        let parts: Vec<&str> = m.as_str().rsplitn(3, '-').collect();
        if parts.len() == 3 && parts[..2].iter().any(|p| p.chars().any(|c| c.is_ascii_digit())) {
            push(m.start(), m.end(), Kind::Pod, 5);
        }
    }
    for m in b.hash.find_iter(text) {
        let s = m.as_str();
        let looks_hex = s.chars().any(|c| c.is_ascii_digit()) && s.chars().any(|c| c.is_ascii_alphabetic());
        if looks_hex {
            push(m.start(), m.end(), Kind::Hash, 6);
        }
    }
    if !servers.is_empty() {
        // Words made of host characters, compared to the known names and hosts. A plain word
        // ("web", "db") is too common in output to be taken for a server.
        let mut start = None;
        for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
            let host_char = c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '@');
            match (host_char, start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => {
                    let word = text[s..i].trim_end_matches('.');
                    let host = word.rsplit('@').next().unwrap_or(word);
                    let hostlike = word.contains('@') || host.contains(|c: char| !c.is_alphabetic());
                    if hostlike && servers.contains(host) {
                        push(s, s + word.len(), Kind::Server, 7);
                    }
                    start = None;
                }
                _ => {}
            }
        }
    }
    out
}

/// Everything recognised on the screen, top to bottom. Overlaps keep the earliest, then longest,
/// then most specific match.
pub fn find(rows: &[Row], servers: &HashSet<String>, custom: &Custom) -> Vec<Match> {
    let mut out = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let mut cands = candidates(&row.text, servers, custom);
        // Hyperlinks (OSC 8) are what the program meant: they come first.
        let byte_at = |ci: usize| row.text.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(row.text.len());
        let mut links = Vec::new();
        for (first, last, uri) in &row.links {
            let (start, end) = (byte_at(*first), byte_at(last + 1));
            if end > start {
                cands.push(Candidate { start, end, kind: Kind::Url, rank: 0 });
                links.push((start, uri.clone()));
            }
        }
        cands.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)).then(a.rank.cmp(&b.rank)));
        let mut taken_until = 0;
        // Custom patterns win over built-in ones wherever they overlap.
        let customs: Vec<(usize, usize)> =
            cands.iter().filter(|c| matches!(c.kind, Kind::Custom(_))).map(|c| (c.start, c.end)).collect();
        for c in cands {
            if c.start < taken_until {
                continue;
            }
            if !matches!(c.kind, Kind::Custom(_)) && customs.iter().any(|&(s, e)| c.start < e && s < c.end) {
                continue;
            }
            taken_until = c.end;
            let first = row.text[..c.start].chars().count();
            let n = row.text[c.start..c.end].chars().count();
            let (Some(&col), Some(&last_col)) = (row.cols.get(first), row.cols.get(first + n - 1)) else { continue };
            let last_w = row.text[c.start..c.end].chars().last().and_then(|ch| ch.width()).unwrap_or(1).max(1) as u16;
            let link = links.iter().find(|(s, _)| *s == c.start && c.rank == 0 && c.kind == Kind::Url);
            out.push(Match {
                row: r as u16,
                col,
                width: last_col + last_w - col,
                text: link.map(|(_, uri)| uri.clone()).unwrap_or_else(|| row.text[c.start..c.end].to_string()),
                kind: c.kind,
            });
        }
    }
    out
}

/// Give each distinct text a label, the bottom of the screen (nearest the prompt) first. All
/// labels have the same length, so none is the start of another.
pub fn label(matches: Vec<Match>, alphabet: &str) -> Vec<Hint> {
    let letters: Vec<char> = {
        let mut seen = HashSet::new();
        alphabet.chars().filter(|c| c.is_ascii_lowercase() && seen.insert(*c)).collect()
    };
    let letters = if letters.len() < 2 { "asdfqwerzxcvjklmiuopghtybn".chars().collect() } else { letters };
    let mut distinct: Vec<&str> = Vec::new();
    for m in matches.iter().rev() {
        if !distinct.contains(&m.text.as_str()) {
            distinct.push(&m.text);
        }
    }
    let n = letters.len();
    let width = if distinct.len() <= n { 1 } else { 2 };
    let capacity = if width == 1 { n } else { n * n };
    let mut labels: HashMap<String, String> = HashMap::new();
    for (i, text) in distinct.iter().take(capacity).enumerate() {
        let l = if width == 1 { letters[i].to_string() } else { format!("{}{}", letters[i / n], letters[i % n]) };
        labels.insert(text.to_string(), l);
    }
    matches.into_iter().filter_map(|m| labels.get(&m.text).cloned().map(|label| Hint { label, m })).collect()
}

/// `src/main.rs:42:7` → (`src/main.rs`, Some(42)).
pub fn split_line(path: &str) -> (&str, Option<u32>) {
    let mut parts = path.splitn(3, ':');
    let file = parts.next().unwrap_or(path);
    let line = parts.next().and_then(|l| l.parse().ok());
    (file, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row of plain text, without links.
    fn plain(text: &str) -> Row {
        let mut cols = Vec::new();
        let mut c = 0u16;
        for ch in text.chars() {
            cols.push(c);
            c += ch.width().unwrap_or(0).max(1) as u16;
        }
        Row { text: text.to_string(), cols, links: Vec::new() }
    }

    fn kinds(line: &str) -> Vec<(String, Kind)> {
        let servers: HashSet<String> = ["web-prod-1".to_string(), "db.lan".to_string()].into();
        find(&[plain(line)], &servers, &Custom::default()).into_iter().map(|m| (m.text, m.kind)).collect()
    }

    #[test]
    fn recognises_common_things() {
        assert_eq!(
            kinds("see https://example.com/a_(b)). or ./src/main.rs:42:7"),
            vec![("https://example.com/a_(b)".into(), Kind::Url), ("./src/main.rs:42:7".into(), Kind::Path)]
        );
        assert_eq!(
            kinds("commit 3f58e39c8 by x, uuid 123e4567-e89b-12d3-a456-426614174000"),
            vec![("3f58e39c8".into(), Kind::Hash), ("123e4567-e89b-12d3-a456-426614174000".into(), Kind::Uuid)]
        );
        assert_eq!(
            kinds("listening on 0.0.0.0:8080 and localhost:5173, peer 10.1.2.3, bad 999.1.1.1"),
            vec![
                ("0.0.0.0:8080".into(), Kind::Address),
                ("localhost:5173".into(), Kind::Address),
                ("10.1.2.3".into(), Kind::Ip)
            ]
        );
        assert_eq!(
            kinds("api-7d4b9c8f5-x2x9z   1/1   Running   my-long-hyphenated-words-here"),
            vec![("api-7d4b9c8f5-x2x9z".into(), Kind::Pod)]
        );
        assert_eq!(
            kinds("ssh deploy@web-prod-1 then db.lan."),
            vec![("deploy@web-prod-1".into(), Kind::Server), ("db.lan".into(), Kind::Server)]
        );
        let servers: HashSet<String> = ["web".to_string()].into();
        assert!(find(&[plain("the web server and admin@web")], &servers, &Custom::default()).len() == 1);
        assert_eq!(loopback_port("http://localhost:5173/app?x=1"), Some((5173, "/app?x=1".into())));
        assert_eq!(loopback_port("https://[::1]:8443"), Some((8443, String::new())));
        assert_eq!(loopback_port("http://localhost:3000?x=1"), Some((3000, "?x=1".into())));
        assert_eq!(loopback_port("http://127.0.0.1:3000#top"), Some((3000, "#top".into())));
        assert_eq!(loopback_port("https://example.com:8443/"), None);
        assert_eq!(kinds("error in lib.rs:12"), vec![("lib.rs:12".into(), Kind::Path)]);
        assert_eq!(kinds("/etc/nginx/nginx.conf"), vec![("/etc/nginx/nginx.conf".into(), Kind::Path)]);
        assert!(kinds("1/2 and/or 2026/10/11 deadbeef 1234567").is_empty(), "{:?}", kinds("1/2 and/or 2026/10/11"));
    }

    #[test]
    fn custom_patterns_win() {
        let cfg = HintsConfig {
            alphabet: "asdf".into(),
            patterns: [
                ("ticket".to_string(), PatternDef { pattern: r"OPS-\d+".into(), tool: Some("jira".into()) }),
                ("bad".to_string(), PatternDef { pattern: "(".into(), tool: None }),
            ]
            .into(),
        };
        let (custom, warnings) = Custom::build(&cfg, &BTreeMap::new());
        assert_eq!(warnings.len(), 2, "a bad regex and an unknown tool: {warnings:?}");
        let m = find(&[plain("fix OPS-1234 now")], &HashSet::new(), &custom);
        assert_eq!(m[0].text, "OPS-1234");
        assert_eq!(m[0].kind, Kind::Custom(0));
    }

    #[test]
    fn hyperlinks_win() {
        let mut row = plain("see the docs here");
        row.links.push((8, 11, "https://example.com/docs".into()));
        let m = find(&[row], &HashSet::new(), &Custom::default());
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].text.as_str(), m[0].col, m[0].width), ("https://example.com/docs", 8, 4));
    }

    #[test]
    fn columns_follow_wide_characters() {
        let m = find(&[plain("日本 https://x.org")], &HashSet::new(), &Custom::default());
        assert_eq!((m[0].col, m[0].width), (5, 13));
    }

    #[test]
    fn labels_are_unique_per_text_and_bottom_first() {
        let rows: Vec<Row> = ["https://a.org", "https://b.org", "https://a.org"].iter().map(|s| plain(s)).collect();
        let hints = label(find(&rows, &HashSet::new(), &Custom::default()), "asdf");
        let labels: Vec<(&str, &str)> = hints.iter().map(|h| (h.m.text.as_str(), h.label.as_str())).collect();
        assert_eq!(labels, vec![("https://a.org", "a"), ("https://b.org", "s"), ("https://a.org", "a")]);
        let many: Vec<Row> = (0..30).map(|i| plain(&format!("https://h{i}.org"))).collect();
        let hints = label(find(&many, &HashSet::new(), &Custom::default()), "ab");
        assert_eq!(hints.len(), 4, "two letters of two: 4 labels");
        assert!(hints.iter().all(|h| h.label.len() == 2));
        assert_eq!(split_line("src/a.rs:42:7"), ("src/a.rs", Some(42)));
        assert_eq!(split_line("/etc/hosts"), ("/etc/hosts", None));
    }
}
