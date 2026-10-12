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
/// which may not be a POSIX one. Each shell quotes in its own way (fish reads `\\` and `\'`
/// inside single quotes, csh refuses a newline there), so the script travels as `printf` octal
/// escapes: the login shell sees only letters, digits, a few marks and backslashes before a
/// digit, which every one of them passes on to sh as they are, and sh rebuilds the script byte
/// for byte.
pub fn sh_c(script: &str) -> String {
    format!("sh -c 'eval \"$(printf \"{}\")\"'", printf_escape(script, true))
}

/// `s` as a `printf` format that prints it: plain characters as they are, every other byte
/// (spaces too unless `spaces`) as a three-digit octal escape. Safe inside double quotes for sh,
/// and inside single quotes for any login shell.
pub fn printf_escape(s: &str, spaces: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_./:=,+@#()[]{}<>;&|*?~^".contains(&b) || (spaces && b == b' ') {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\{b:03o}"));
        }
    }
    out
}

/// The script an [`sh_c`] line carries.
#[cfg(test)]
pub fn unwrap_sh_c(line: &str) -> String {
    let inner = line.strip_prefix("sh -c 'eval \"$(printf \"").and_then(|l| l.strip_suffix("\")\"'"));
    let inner = inner.expect("an sh -c line").as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < inner.len() {
        if inner[i] == b'\\' {
            let octal = std::str::from_utf8(&inner[i + 1..i + 4]).unwrap();
            out.push(u8::from_str_radix(octal, 8).unwrap());
            i += 4;
        } else {
            out.push(inner[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
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
        assert_eq!(unwrap_sh_c(&sh_c(script)), script);
        // What sh makes of it is the original text.
        for s in ["it's", "a\"b", "$(x) `y` \\z", "new\nline", ""] {
            let out =
                std::process::Command::new("sh").arg("-c").arg(format!("printf %s {}", quote(s))).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), s);
        }
    }

    /// Whatever the login shell on the server (sh, bash, zsh, fish, csh), sh gets the script
    /// byte for byte: backslashes, quotes, `!`, newlines, `%` and non-ASCII names included.
    #[test]
    fn a_script_reaches_sh_whole_through_any_login_shell() {
        let name = "a\\\\b \\' \"q\" $(x) `y` !1 %s\n\té.txt";
        let script = format!("printf '%s' {}; printf '|%s' \"$0\"", quote(name));
        let line = sh_c(&script);
        // What the login shell parses: nothing it could read in its own way.
        let inner = &line["sh -c '".len()..line.len() - 1];
        assert!(!inner.contains(['\'', '\n', '!', '%']), "{line}");
        assert!(!inner.contains("\\\\"), "{line}");
        assert!(inner.split('\\').skip(1).all(|p| p.starts_with(|c: char| c.is_ascii_digit())), "{line}");
        assert_eq!(unwrap_sh_c(&line), script);
        let shells: Vec<&str> = ["sh", "bash", "zsh", "fish", "tcsh", "csh"].into_iter().filter(|s| have(s)).collect();
        assert!(shells.contains(&"sh"));
        for shell in shells {
            let out = std::process::Command::new(shell).arg("-c").arg(&line).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{name}|sh"), "through {shell}");
        }
    }

    fn have(program: &str) -> bool {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {program} >/dev/null"))
            .status()
            .is_ok_and(|s| s.success())
    }
}
