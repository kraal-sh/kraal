//! Quoting for POSIX shells. Every command line kraal builds for `sh -c`, ssh or a pane goes
//! through here, so there is one way to get it right.

/// `s` as one shell word: unchanged when it holds only plain characters, single-quoted otherwise.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@,+%".contains(c)) {
        return s.to_string();
    }
    quote_always(s)
}

/// `s` single-quoted, even when it would not need it.
pub fn quote_always(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `sh -c '<script>'`: a script for a login shell on a server (what ssh runs a command with),
/// which may not be a POSIX one (fish, csh). Each of them passes one quoted word on to sh.
pub fn sh_c(script: &str) -> String {
    format!("sh -c {}", quote_always(script))
}

/// Several words, quoted and joined into one command line.
pub fn join(words: &[String]) -> String {
    words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote("plain-path/a.b"), "plain-path/a.b");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("$HOME"), "'$HOME'");
        assert_eq!(quote_always("x"), "'x'");
        assert_eq!(join(&["echo".into(), "a b".into()]), "echo 'a b'");
        // What a login shell hands to sh is the script itself.
        let script = "x=$(echo 'a b'); for w in $x; do printf '[%s]' \"$w\"; done";
        let out = std::process::Command::new("sh").arg("-c").arg(sh_c(script)).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "[a][b]");
        // What sh makes of it is the original text.
        for s in ["it's", "a\"b", "$(x) `y` \\z", "new\nline", ""] {
            let out =
                std::process::Command::new("sh").arg("-c").arg(format!("printf %s {}", quote(s))).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), s);
        }
    }
}
