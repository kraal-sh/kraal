//! Paste buffers, as tmux keeps them: every copy is also kept by the session server, newest
//! first, to paste again later. Numbered buffers rotate (at most `limit` of them, and the oldest
//! go first when all buffers together hold more than `max_bytes`); named ones, set from the
//! command line, stay until deleted. They live in memory, unless the configuration asks to keep
//! them in a private file across restarts.

use serde::{Deserialize, Serialize};

/// Longest name a buffer can have.
const NAME_MAX: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Buffer {
    /// Given when it was set; a numbered buffer has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub text: String,
    /// When it was copied or set, in seconds since the epoch.
    pub at: i64,
    /// The same as long as the buffer exists, whatever comes before it: what a list on screen
    /// points at.
    #[serde(skip)]
    pub id: u64,
}

/// What is written when the buffers are kept across restarts.
#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    buffers: Vec<Buffer>,
}

const SAVED_VERSION: u32 = 1;

pub struct Buffers {
    /// Newest first.
    list: Vec<Buffer>,
    /// Numbered buffers kept at most.
    limit: usize,
    /// Bytes all buffers hold together at most.
    max_bytes: usize,
    next_id: u64,
}

impl Buffers {
    pub fn new(limit: usize, max_bytes: usize) -> Buffers {
        Buffers { list: Vec::new(), limit, max_bytes, next_id: 1 }
    }

    /// New limits (the configuration changed): what no longer fits goes, the oldest first.
    pub fn set_limits(&mut self, limit: usize, max_bytes: usize) {
        self.limit = limit;
        self.max_bytes = max_bytes;
        self.trim();
    }

    /// Newest first.
    pub fn list(&self) -> &[Buffer] {
        &self.list
    }

    /// What commands call the buffer at `index` of the list: its name, or its number among the
    /// numbered buffers (0 is the newest).
    pub fn key(&self, index: usize) -> String {
        let Some(b) = self.list.get(index) else { return String::new() };
        match &b.name {
            Some(name) => name.clone(),
            None => self.list[..index].iter().filter(|b| b.name.is_none()).count().to_string(),
        }
    }

    pub fn by_id(&self, id: u64) -> Option<&Buffer> {
        self.list.iter().find(|b| b.id == id)
    }

    /// [`Buffers::key`] of the buffer with this id.
    pub fn key_of(&self, id: u64) -> Option<String> {
        self.list.iter().position(|b| b.id == id).map(|i| self.key(i))
    }

    /// Keep a copy as the newest numbered buffer. The same text as the newest numbered buffer
    /// is not kept twice: that one becomes the newest again. Whether anything changed.
    pub fn copy(&mut self, text: &str, at: i64) -> bool {
        if text.is_empty() || self.limit == 0 || text.len() > self.max_bytes {
            return false;
        }
        if let Some(i) = self.list.iter().position(|b| b.name.is_none())
            && self.list[i].text == text
        {
            let mut b = self.list.remove(i);
            b.at = at;
            self.list.insert(0, b);
            return true;
        }
        self.push(None, text.to_string(), at);
        true
    }

    /// Keep `text` in the buffer `name` (replacing one of that name), or in a new numbered
    /// buffer without a name.
    pub fn set(&mut self, name: Option<&str>, text: String, at: i64) -> Result<(), String> {
        let most = crate::text::human_size(self.max_bytes as u64);
        if text.len() > self.max_bytes {
            return Err(format!("larger than buffers.max_mb ({most})"));
        }
        let Some(name) = name else {
            if self.limit == 0 {
                return Err("buffers.limit is 0: no numbered buffer is kept (give it a name)".into());
            }
            self.copy(&text, at);
            return Ok(());
        };
        check_name(name)?;
        let named: usize =
            self.list.iter().filter(|b| b.name.as_deref().is_some_and(|n| n != name)).map(|b| b.text.len()).sum();
        if named + text.len() > self.max_bytes {
            return Err(format!("named buffers would hold more than buffers.max_mb ({most}): delete some first"));
        }
        self.list.retain(|b| b.name.as_deref() != Some(name));
        self.push(Some(name.to_string()), text, at);
        Ok(())
    }

    fn push(&mut self, name: Option<String>, text: String, at: i64) {
        let id = self.next_id;
        self.next_id += 1;
        self.list.insert(0, Buffer { name, text, at, id });
        self.trim();
    }

    /// Drop the oldest numbered buffers beyond the limits.
    fn trim(&mut self) {
        let mut numbered = self.list.iter().filter(|b| b.name.is_none()).count();
        let mut bytes: usize = self.list.iter().map(|b| b.text.len()).sum();
        while numbered > 0 && (numbered > self.limit || bytes > self.max_bytes) {
            let Some(i) = self.list.iter().rposition(|b| b.name.is_none()) else { break };
            bytes -= self.list.remove(i).text.len();
            numbered -= 1;
        }
    }

    /// The buffer a command names (see [`Buffers::key`]); the newest without one.
    pub fn get(&self, key: Option<&str>) -> Result<&Buffer, String> {
        self.index(key).map(|i| &self.list[i])
    }

    fn index(&self, key: Option<&str>) -> Result<usize, String> {
        let Some(key) = key else {
            return if self.list.is_empty() { Err("no paste buffers yet".into()) } else { Ok(0) };
        };
        match key.parse::<usize>() {
            Ok(n) => {
                let numbered: Vec<usize> = (0..self.list.len()).filter(|i| self.list[*i].name.is_none()).collect();
                numbered.get(n).copied().ok_or_else(|| match numbered.len() {
                    0 => format!("no buffer {n}: there is no numbered buffer"),
                    1 => format!("no buffer {n}: there is one numbered buffer, 0"),
                    count => format!("no buffer {n}: numbered buffers go from 0 (the newest) to {}", count - 1),
                })
            }
            Err(_) => {
                self.list.iter().position(|b| b.name.as_deref() == Some(key)).ok_or(format!("no buffer named {key:?}"))
            }
        }
    }

    /// Delete the buffer a command names; the newest without one.
    pub fn delete(&mut self, key: Option<&str>) -> Result<Buffer, String> {
        let i = self.index(key)?;
        Ok(self.list.remove(i))
    }

    pub fn delete_id(&mut self, id: u64) -> Option<Buffer> {
        let i = self.list.iter().position(|b| b.id == id)?;
        Some(self.list.remove(i))
    }

    /// Delete every buffer; how many there were.
    pub fn clear(&mut self) -> usize {
        std::mem::take(&mut self.list).len()
    }

    /// The buffers as kept across restarts.
    pub fn to_json(&self) -> Vec<u8> {
        let saved = Saved { version: SAVED_VERSION, buffers: self.list.clone() };
        serde_json::to_vec(&saved).unwrap_or_default()
    }

    /// Take back buffers kept across restarts, within the limits.
    pub fn load_json(&mut self, data: &[u8]) -> Result<(), String> {
        let saved: Saved = serde_json::from_slice(data).map_err(|e| e.to_string())?;
        if saved.version != SAVED_VERSION {
            return Err(format!("unknown version {}", saved.version));
        }
        let mut list: Vec<Buffer> = Vec::new();
        for mut b in saved.buffers {
            // A name kraal would not take, or a second buffer of the same name, is dropped.
            if b.name.as_deref().is_some_and(|n| check_name(n).is_err() || list.iter().any(|o| o.name == b.name)) {
                continue;
            }
            b.id = self.next_id;
            self.next_id += 1;
            list.push(b);
        }
        self.list = list;
        self.trim();
        Ok(())
    }
}

/// Whether kraal takes `name` for a buffer: one word of letters, digits and `- _ . : @ +`, not
/// a number (numbers name the numbered buffers).
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().count() > NAME_MAX {
        return Err(format!("a buffer name has 1 to {NAME_MAX} characters"));
    }
    if name.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("{name:?} is a number: numbers name the numbered buffers"));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@' | '+')) {
        return Err(format!("{name:?}: a buffer name has letters, digits and - _ . : @ + only"));
    }
    Ok(())
}

/// The text on one line, as a list shows it: its lines joined by `sep`, control characters
/// made visible, cut to `width` columns.
pub fn one_line(text: &str, sep: &str, width: usize) -> String {
    let mut line = String::new();
    for (i, l) in text.trim().lines().enumerate() {
        if i > 0 {
            line.push_str(sep);
        }
        line.push_str(&crate::text::sanitize(l.trim_end()));
        if crate::text::columns(&line) > width {
            break;
        }
    }
    crate::text::truncate(&line, width)
}

/// How long ago something happened, as a list shows it: 45s, 12m, 3h, 2d.
pub fn age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// How many lines a buffer's text has (a last line without its line end counts).
pub fn lines(text: &str) -> usize {
    text.lines().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(b: &Buffers) -> Vec<(String, &str)> {
        (0..b.list().len()).map(|i| (b.key(i), b.list()[i].text.as_str())).collect()
    }

    #[test]
    fn copies_rotate_within_the_count_and_the_size() {
        let mut b = Buffers::new(3, 100);
        for (i, t) in ["one", "two", "three", "four"].iter().enumerate() {
            assert!(b.copy(t, i as i64));
        }
        assert_eq!(
            texts(&b),
            [("0".into(), "four"), ("1".into(), "three"), ("2".into(), "two")],
            "the oldest went beyond 3"
        );
        // 60 + 50 bytes are more than 100: the older one goes.
        b.copy(&"a".repeat(60), 10);
        b.copy(&"b".repeat(50), 11);
        assert_eq!(b.list().len(), 1);
        assert_eq!(b.list()[0].text, "b".repeat(50));
        // A copy larger than everything is not kept, and nothing else goes for it.
        assert!(!b.copy(&"c".repeat(101), 12));
        assert_eq!(b.list().len(), 1);
        // Nothing is kept of an empty copy, or when the limit is 0.
        assert!(!b.copy("", 13));
        let mut none = Buffers::new(0, 100);
        assert!(!none.copy("x", 0) && none.list().is_empty());
        assert!(none.set(None, "x".into(), 0).is_err());
        none.set(Some("kept"), "x".into(), 0).unwrap();
        assert_eq!(none.list().len(), 1);
        // Smaller limits apply at once.
        let mut b = Buffers::new(5, 100);
        for t in ["a", "b", "c", "d"] {
            b.copy(t, 0);
        }
        b.set_limits(2, 100);
        assert_eq!(texts(&b), [("0".into(), "d"), ("1".into(), "c")]);
    }

    #[test]
    fn the_same_copy_twice_in_a_row_is_kept_once() {
        let mut b = Buffers::new(10, 1000);
        b.copy("same", 1);
        let id = b.list()[0].id;
        assert!(b.copy("same", 5));
        assert_eq!(b.list().len(), 1);
        assert_eq!((b.list()[0].id, b.list()[0].at), (id, 5), "the same buffer, copied again later");
        b.copy("other", 6);
        b.copy("same", 7);
        assert_eq!(texts(&b), [("0".into(), "same"), ("1".into(), "other"), ("2".into(), "same")]);
        // A named buffer with the same text does not count: it is not a copy.
        b.set(Some("note"), "again".into(), 8).unwrap();
        b.copy("again", 9);
        assert_eq!(b.list().len(), 5);
        // Nor does a numbered buffer that is not the newest numbered one.
        b.copy("same", 10);
        assert_eq!(b.list().iter().filter(|x| x.text == "same").count(), 3);
    }

    #[test]
    fn named_buffers_stay_and_numbers_count_only_the_numbered() {
        let mut b = Buffers::new(2, 1000);
        b.copy("first", 1);
        b.set(Some("notes"), "my notes".into(), 2).unwrap();
        b.copy("second", 3);
        b.copy("third", 4);
        assert_eq!(texts(&b), [("0".into(), "third"), ("1".into(), "second"), ("notes".into(), "my notes")]);
        assert_eq!(b.get(Some("1")).unwrap().text, "second");
        assert_eq!(b.get(Some("notes")).unwrap().text, "my notes");
        assert_eq!(b.get(None).unwrap().text, "third", "the newest without a name");
        assert!(b.get(Some("2")).unwrap_err().contains("0 (the newest) to 1"));
        assert!(b.get(Some("nope")).is_err());
        // Setting a name again replaces it, and makes it the newest.
        b.set(Some("notes"), "new notes".into(), 5).unwrap();
        assert_eq!(b.list().iter().filter(|x| x.name.is_some()).count(), 1);
        assert_eq!(b.key(0), "notes");
        assert_eq!(b.get(None).unwrap().text, "new notes");
        // Many copies later, the named one is still there.
        for i in 0..10 {
            b.copy(&format!("copy {i}"), 10 + i);
        }
        assert_eq!(b.get(Some("notes")).unwrap().text, "new notes");
        assert_eq!(b.list().len(), 3);
        // Numbers are not names, and names are one word.
        for bad in ["12", "", "two words", "a/b", &"x".repeat(65)] {
            assert!(b.set(Some(bad), "x".into(), 0).is_err(), "{bad:?}");
        }
        b.set(Some("ssh-key.v2"), "x".into(), 0).unwrap();
        // Named buffers are bounded by the size too, never dropped for it.
        let mut small = Buffers::new(5, 10);
        small.copy("12345", 0);
        small.set(Some("a"), "1234567".into(), 1).unwrap();
        assert_eq!(texts(&small), [("a".into(), "1234567")], "the numbered one made room");
        assert!(small.set(Some("b"), "1234".into(), 2).is_err());
        small.set(Some("a"), "0123456789".into(), 3).unwrap();
        assert!(small.set(None, "0123456789x".into(), 4).is_err());
    }

    #[test]
    fn delete_by_number_name_or_the_newest() {
        let mut b = Buffers::new(10, 1000);
        b.copy("a", 0);
        b.copy("b", 1);
        b.set(Some("n"), "named".into(), 2).unwrap();
        assert_eq!(b.delete(Some("1")).unwrap().text, "a");
        assert_eq!(b.delete(None).unwrap().text, "named");
        let id = b.list()[0].id;
        assert_eq!(b.key_of(id).as_deref(), Some("0"));
        assert!(b.delete_id(id).is_some() && b.list().is_empty());
        assert!(b.delete(None).is_err());
        b.copy("x", 0);
        b.copy("y", 0);
        assert_eq!(b.clear(), 2);
    }

    #[test]
    fn buffers_kept_across_restarts_come_back_within_the_limits() {
        let mut b = Buffers::new(10, 1000);
        b.copy("old", 1);
        b.set(Some("notes"), "named".into(), 2).unwrap();
        b.copy("new", 3);
        let json = b.to_json();
        let mut again = Buffers::new(1, 1000);
        again.load_json(&json).unwrap();
        assert_eq!(texts(&again), [("0".into(), "new"), ("notes".into(), "named")]);
        assert!(again.list().iter().all(|x| x.id > 0));
        assert!(again.load_json(br#"{"version":9,"buffers":[]}"#).is_err());
        assert!(again.load_json(b"not json").is_err());
    }

    #[test]
    fn lists_show_one_line_and_an_age() {
        assert_eq!(one_line("  first\nsecond\x1b[31m\n", " | ", 40), "first | second\u{241b}[31m");
        assert_eq!(one_line(&"x".repeat(50), " | ", 10).chars().count(), 10);
        assert_eq!(age(5), "5s");
        assert_eq!(age(125), "2m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(200_000), "2d");
        assert_eq!(age(-3), "0s");
        assert_eq!(lines("a\nb\n"), 2);
        assert_eq!(lines("a\nb"), 2);
    }
}
