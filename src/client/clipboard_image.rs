//! Clipboard reads require a local confirmation and share the attachment worker's cancellation.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::attachments::IMAGE_BYTES as MAX_BYTES;
const TIMEOUT: Duration = Duration::from_secs(2);
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

pub(super) struct Image {
    pub path: PathBuf,
}

impl Image {
    fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kraal-clipboard-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        Ok(Self { path: root.join("clipboard.png") })
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.path.parent().unwrap());
    }
}

fn validate(bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= MAX_BYTES, "clipboard image exceeds 16 MiB; save it and use Attach files");
    ensure!(bytes.starts_with(PNG), "clipboard has no PNG image; copy an image first");
    Ok(())
}

fn run(cmd: &mut Command, cancelled: &AtomicBool, limit: usize, timeout: Duration) -> Result<Vec<u8>> {
    ensure!(!cancelled.load(Ordering::SeqCst), "clipboard read cancelled");
    let out = crate::exec::run_cancellable(cmd, timeout, limit, cancelled).context("cannot start clipboard tool")?;
    ensure!(!out.cancelled && !cancelled.load(Ordering::SeqCst), "clipboard read cancelled");
    ensure!(!out.timed_out, "clipboard tool timed out; copy the image again");
    ensure!(!out.truncated, "clipboard image exceeds 16 MiB; save it and use Attach files");
    ensure!(out.success(), "clipboard tool could not read a PNG image; copy an image first");
    Ok(out.stdout)
}

#[cfg(not(target_os = "macos"))]
fn commands(present: impl Fn(&str) -> bool) -> Vec<Command> {
    let mut commands = Vec::new();
    if present("WAYLAND_DISPLAY") {
        let mut command = Command::new("wl-paste");
        command.args(["--no-newline", "--type", "image/png"]);
        commands.push(command);
    }
    if present("DISPLAY") {
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard", "-out", "-target", "image/png"]);
        commands.push(command);
    }
    commands
}

#[cfg(not(target_os = "macos"))]
fn read_commands(commands: Vec<Command>, cancelled: &AtomicBool) -> Result<Vec<u8>> {
    let mut error = anyhow::anyhow!("no graphical clipboard available; run attach on your desktop");
    for mut command in commands {
        let result = run(&mut command, cancelled, MAX_BYTES, TIMEOUT).and_then(|bytes| {
            validate(&bytes)?;
            Ok(bytes)
        });
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(e) => error = e,
        }
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
    }
    Err(error.context("Cannot read clipboard image (wl-paste or xclip required)"))
}

pub(super) fn read(cancelled: &AtomicBool) -> Result<Image> {
    let image = Image::new()?;
    #[cfg(not(target_os = "macos"))]
    {
        let bytes = read_commands(commands(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty())), cancelled)?;
        crate::paths::write_private(&image.path, &bytes)?;
    }
    #[cfg(target_os = "macos")]
    {
        // The private, precreated file is passed as an argument, never interpolated into AppleScript.
        crate::paths::write_private(&image.path, b"")?;
        let mut command = Command::new("osascript");
        command.args(["-e", EXPORT, "--"]).arg(&image.path);
        limit_export(&mut command);
        let result = run(&mut command, cancelled, 4096, TIMEOUT);
        use std::io::Read;
        let file = fs::File::open(&image.path)?;
        ensure!(
            file.metadata()?.len() <= MAX_BYTES as u64,
            "clipboard image exceeds 16 MiB; save it and use Attach files"
        );
        result?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        validate(&bytes)?;
    }
    ensure!(!cancelled.load(Ordering::SeqCst), "clipboard read cancelled");
    Ok(image)
}

#[cfg(any(target_os = "macos", test))]
fn limit_export(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Bound the export while it is written, including a byte that distinguishes overflow.
    // A size-limit signal must not dump clipboard contents into a core file.
    // SAFETY: the child only calls async-signal-safe setrlimit before exec.
    unsafe {
        command.pre_exec(|| {
            let bytes = (MAX_BYTES + 1) as libc::rlim_t;
            let limit = libc::rlimit { rlim_cur: bytes, rlim_max: bytes };
            let core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if libc::setrlimit(libc::RLIMIT_CORE, &core) == 0 && libc::setrlimit(libc::RLIMIT_FSIZE, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(target_os = "macos")]
const EXPORT: &str = r#"on run argv
    set imageData to the clipboard as «class PNGf»
    set outputFile to open for access POSIX file (item 1 of argv) with write permission
    try
        set eof of outputFile to 0
        write imageData to outputFile
        close access outputFile
    on error messageText number messageNumber
        close access outputFile
        error messageText number messageNumber
    end try
end run"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn png_export_cannot_write_past_the_limit_and_failure_removes_its_file() {
        let image = Image::new().unwrap();
        let path = image.path.clone();
        let mut command = shell("head -c 16777218 /dev/zero > \"$1\"");
        command.arg("sh").arg(&path);
        limit_export(&mut command);
        assert!(run(&mut command, &AtomicBool::new(false), 4096, crate::testing::PATIENCE).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_BYTES as u64 + 1);
        drop(image);
        assert!(!path.exists());
    }

    #[test]
    fn png_size_and_signature_are_checked_without_truncation() {
        let mut bytes = vec![0; MAX_BYTES];
        bytes[..PNG.len()].copy_from_slice(PNG);
        validate(&bytes).unwrap();
        bytes.push(0);
        assert!(validate(&bytes).unwrap_err().to_string().contains("16 MiB"));
        for bytes in [b"".as_slice(), b"text", b"\x89PNG"] {
            assert!(validate(bytes).is_err());
        }
    }

    #[test]
    fn staging_is_private_and_removed_when_the_read_or_transfer_ends() {
        let image = Image::new().unwrap();
        let path = image.path.clone();
        crate::paths::write_private(&path, PNG).unwrap();
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        drop(image);
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn helpers_fail_visibly_on_missing_tools_failure_timeout_overflow_and_cancellation() {
        let stop = AtomicBool::new(false);
        assert!(run(&mut Command::new("/no-such-clipboard-tool"), &stop, 16, TIMEOUT).is_err());
        assert!(run(&mut shell("exit 1"), &stop, 16, TIMEOUT).is_err());
        assert!(
            run(&mut shell("sleep 60"), &stop, 16, Duration::from_millis(50))
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(run(&mut shell("printf '12345'"), &stop, 4, TIMEOUT).unwrap_err().to_string().contains("16 MiB"));
        assert_eq!(run(&mut shell("printf '1234'"), &stop, 4, TIMEOUT).unwrap(), b"1234");
        stop.store(true, Ordering::SeqCst);
        assert!(run(&mut shell("exit 0"), &stop, 16, TIMEOUT).unwrap_err().to_string().contains("cancelled"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_prefers_wayland_then_x11_clipboard_and_falls_back_on_failure() {
        let both = commands(|_| true);
        assert_eq!(both[0].get_program(), "wl-paste");
        assert_eq!(both[1].get_program(), "xclip");
        assert_eq!(both[1].get_args().collect::<Vec<_>>(), ["-selection", "clipboard", "-out", "-target", "image/png"]);
        assert_eq!(commands(|key| key == "DISPLAY")[0].get_program(), "xclip");
        assert!(commands(|_| false).is_empty());
        let valid = || shell("printf '\\211PNG\\r\\n\\032\\n'");
        let stop = AtomicBool::new(false);
        assert_eq!(read_commands(vec![shell("exit 1"), valid()], &stop).unwrap(), PNG);
        assert_eq!(read_commands(vec![shell("printf text"), valid()], &stop).unwrap(), PNG);
        assert_eq!(read_commands(vec![valid(), shell("exit 1")], &stop).unwrap(), PNG);
        assert!(read_commands(vec![], &stop).is_err());
    }
}
