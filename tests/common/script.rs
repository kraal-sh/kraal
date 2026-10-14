use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// A shell creates its redirected file before writing. Only a trailing newline makes the
/// stand-in's arguments or pid ready to read, even if the write arrived in several pieces.
pub fn read_complete_lines(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok().filter(|text| text.ends_with('\n'))
}

/// Write an executable script at `path`. A child process writes it: a file this process held
/// open for writing could be inherited by a program another test starts at that moment, and
/// running the script would then fail with "text file busy".
pub fn write_script(path: &Path, body: &str) {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat > \"$1\" && chmod 755 \"$1\"")
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success(), "writing {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirected_output_is_ready_only_after_the_line_is_complete() {
        let path = std::env::temp_dir().join(format!("kraal-script-output-{}", std::process::id()));
        assert_eq!(read_complete_lines(&path), None);
        let mut file = std::fs::File::create(&path).unwrap();
        assert_eq!(read_complete_lines(&path), None);
        for part in ["resume ", "source"] {
            file.write_all(part.as_bytes()).unwrap();
            assert_eq!(read_complete_lines(&path), None);
        }
        file.write_all(b"\n").unwrap();
        assert_eq!(read_complete_lines(&path).as_deref(), Some("resume source\n"));
        file.write_all(b"next").unwrap();
        assert_eq!(read_complete_lines(&path), None, "the last line is still being written");
        file.write_all(b"\n").unwrap();
        assert_eq!(read_complete_lines(&path).as_deref(), Some("resume source\nnext\n"));
        drop(file);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"12").unwrap();
        assert_eq!(read_complete_lines(&path), None, "a parseable pid can still be incomplete");
        file.write_all(b"34\n").unwrap();
        assert_eq!(read_complete_lines(&path).as_deref(), Some("1234\n"));
        drop(file);
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(read_complete_lines(&path).as_deref(), Some("\n"), "empty arguments are a complete line");
        std::fs::remove_file(path).unwrap();
    }
}
