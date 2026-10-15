//! Clipboard reads follow a local gesture and share the attachment worker's cancellation.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::attachments::IMAGE_BYTES as MAX_BYTES;
const TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(not(target_os = "macos"))]
mod native;
#[cfg(not(target_os = "macos"))]
mod wayland;
#[cfg(not(target_os = "macos"))]
mod x11;

#[cfg(not(target_os = "macos"))]
const TOO_LARGE: &str = "clipboard image exceeds 16 MiB; save it and use Attach files";
pub(super) const TEXT_TOO_LARGE: &str = "copy too large; raise clipboard.max_bytes and attach again";
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
    ensure!(out.success(), "clipboard tool failed: {}; copy the image again", tool_error(&out));
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
fn read_commands(commands: Vec<Command>, cancelled: &AtomicBool) -> Result<Vec<u8>, Diagnostic> {
    let mut error = None;
    for mut command in commands {
        let result = run(&mut command, cancelled, MAX_BYTES, TIMEOUT).and_then(|bytes| {
            validate(&bytes)?;
            Ok(bytes)
        });
        let failure = match result {
            Ok(bytes) => return Ok(bytes),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Diagnostic::MissingTool(if command.get_program() == "wl-paste" { "wl-clipboard" } else { "xclip" })
            }
            Err(error) => Diagnostic::Failed(format!("{error:#}")),
        };
        if error.is_none() || matches!((&error, &failure), (Some(Diagnostic::MissingTool(_)), Diagnostic::Failed(_))) {
            error = Some(failure);
        }
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
    }
    Err(error.unwrap_or(Diagnostic::NoDisplay))
}

#[derive(Debug)]
pub(super) enum Diagnostic {
    Empty,
    MissingTool(&'static str),
    Failed(String),
    NoDisplay,
    NotAgent,
}

impl Diagnostic {
    fn message(&self) -> String {
        match self {
            Self::Empty => "The clipboard is empty".into(),
            Self::MissingTool(tool) => format!("Ctrl+V could not read the clipboard: install {tool}"),
            Self::Failed(error) => format!("Ctrl+V could not read the clipboard: {error}. Try Ctrl+V again"),
            Self::NoDisplay => "Ctrl+V could not read the clipboard: run attach from your desktop terminal with WAYLAND_DISPLAY or DISPLAY set".into(),
            Self::NotAgent => "Ctrl+V did not attach an image: this pane is not recognized as an agent. Use Attach clipboard image".into(),
        }
    }
}

#[derive(Default)]
pub(super) struct Notices {
    seen: Vec<std::mem::Discriminant<Diagnostic>>,
    current: Option<(String, std::time::Instant)>,
}

impl Notices {
    pub fn is_active(&self) -> bool {
        self.current.is_some()
    }

    pub fn show(&mut self, diagnostic: Diagnostic) -> bool {
        let cause = std::mem::discriminant(&diagnostic);
        if self.seen.contains(&cause) {
            return false;
        }
        self.seen.push(cause);
        self.current = Some((
            diagnostic.message().chars().filter(|c| !c.is_control()).collect(),
            std::time::Instant::now() + Duration::from_secs(4),
        ));
        true
    }

    pub fn clear(&mut self) {
        self.current = None;
    }

    pub fn expire(&mut self, now: std::time::Instant) -> bool {
        if self.current.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.clear();
            return true;
        }
        false
    }

    pub fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        let Some((message, _)) = &self.current else { return Vec::new() };
        let lines = crate::text::wrap(message, usize::from(cols).saturating_sub(2).max(1))
            .into_iter()
            .map(|line| (line, Some(crate::theme::NoticeKind::Notice)))
            .collect::<Vec<_>>();
        super::notice_lines(&lines, cols, rows)
    }
}

// A probe reads only types; image content waits for local authorization.
pub(super) fn available() -> Result<bool, Diagnostic> {
    let cancelled = AtomicBool::new(false);
    #[cfg(target_os = "macos")]
    {
        advertised(probes(), &cancelled, TIMEOUT)
    }
    #[cfg(not(target_os = "macos"))]
    {
        native::select(
            display_present,
            |backend| match backend {
                native::Backend::Wayland => wayland::list_types(&cancelled),
                native::Backend::X11 => x11::list_types(&cancelled),
            },
            || advertised(probes(), &cancelled, TIMEOUT),
        )
    }
}

fn advertised(commands: Vec<Command>, cancelled: &AtomicBool, timeout: Duration) -> Result<bool, Diagnostic> {
    let mut error = None;
    for mut command in commands {
        let tool = command.get_program().to_string_lossy().into_owned();
        let failure = match crate::exec::run_cancellable(&mut command, timeout, 16 * 1024, cancelled) {
            Ok(out) if out.success() && !out.truncated => {
                return Ok(offers_image(&String::from_utf8_lossy(&out.stdout)));
            }
            Ok(out) => {
                let reason = if out.timed_out {
                    "timed out".into()
                } else if out.cancelled {
                    "cancelled".into()
                } else if out.truncated {
                    "type list exceeds 16 KiB".into()
                } else {
                    tool_error(&out)
                };
                Diagnostic::Failed(format!("{tool}: {reason}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Diagnostic::MissingTool(match tool.as_str() {
                "wl-paste" => "wl-clipboard",
                "xclip" => "xclip",
                _ => "osascript (macOS system tools)",
            }),
            Err(e) => Diagnostic::Failed(format!("{tool}: {e}")),
        };
        // Keep the first tool failure, but never suggest installation if another tool ran.
        if error.is_none() || matches!((&error, &failure), (Some(Diagnostic::MissingTool(_)), Diagnostic::Failed(_))) {
            error = Some(failure);
        }
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
    }
    Err(error.unwrap_or(Diagnostic::NoDisplay))
}

fn tool_error(out: &crate::exec::Output) -> String {
    let detail: String = out.stderr_text().chars().filter(|c| !c.is_control()).take(200).collect();
    if detail.trim().is_empty() {
        format!("exited with status {}", out.code.map_or_else(|| "unknown".into(), |code| code.to_string()))
    } else {
        detail.trim().into()
    }
}

#[cfg(not(target_os = "macos"))]
fn display_present(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|value| !value.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn probes() -> Vec<Command> {
    probe_commands(display_present)
}

#[cfg(not(target_os = "macos"))]
fn probe_commands(present: impl Fn(&str) -> bool) -> Vec<Command> {
    let mut commands = Vec::new();
    if present("WAYLAND_DISPLAY") {
        let mut command = Command::new("wl-paste");
        command.arg("--list-types");
        commands.push(command);
    }
    if present("DISPLAY") {
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard", "-out", "-target", "TARGETS"]);
        commands.push(command);
    }
    commands
}

#[cfg(target_os = "macos")]
fn probes() -> Vec<Command> {
    let mut command = Command::new("osascript");
    command.args(["-e", "clipboard info"]);
    vec![command]
}

fn offers_image(types: &str) -> bool {
    #[cfg(not(target_os = "macos"))]
    {
        types.lines().any(|line| line.trim() == "image/png")
    }
    #[cfg(target_os = "macos")]
    {
        ["PNGf", "TIFF", "JPEG", "GIFf", "PICT"].iter().any(|kind| types.contains(kind))
    }
}

#[derive(Default, Clone)]
pub(super) struct Keys {
    parser: crate::input::InputParser,
    raw: Vec<u8>,
}

impl Keys {
    pub fn control_v_end(&self, bytes: &[u8]) -> Option<usize> {
        let mut parser = self.clone();
        for (index, byte) in bytes.iter().enumerate() {
            if parser.feed(std::slice::from_ref(byte)).iter().any(|(_, control_v)| *control_v) {
                return Some(index + 1);
            }
        }
        None
    }

    pub fn pending(&self) -> bool {
        !self.raw.is_empty()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(Vec<u8>, bool)> {
        use crate::input::{InputEvent, KeyCode, Mods};
        let mut parts = Vec::new();
        for index in 0..bytes.len().max(1) {
            let events = if bytes.is_empty() {
                self.parser.flush()
            } else {
                self.raw.push(bytes[index]);
                self.parser.feed(&bytes[index..=index])
            };
            for event in &events {
                if let InputEvent::Key(key) = event
                    && key.code == KeyCode::Char('v')
                    && key.mods == Mods::CTRL
                    && self.raw.ends_with(&key.raw)
                {
                    let key_bytes = self.raw.split_off(self.raw.len() - key.raw.len());
                    if !self.raw.is_empty() {
                        parts.push((std::mem::take(&mut self.raw), false));
                    }
                    parts.push((key_bytes, true));
                }
            }
            if (!events.is_empty() || !self.parser.has_pending()) && !self.raw.is_empty() {
                if let Some((raw, false)) = parts.last_mut() {
                    raw.append(&mut self.raw);
                } else {
                    parts.push((std::mem::take(&mut self.raw), false));
                }
            }
        }
        parts
    }
}

pub(super) fn read(cancelled: &AtomicBool) -> Result<Image> {
    let image = Image::new()?;
    #[cfg(not(target_os = "macos"))]
    {
        let bytes = native::select(
            display_present,
            |backend| match backend {
                native::Backend::Wayland => wayland::read_png(cancelled),
                native::Backend::X11 => x11::read_png(cancelled),
            },
            || read_commands(commands(display_present), cancelled),
        )
        .map_err(|diagnostic| anyhow::anyhow!(diagnostic.message()))?;
        validate(&bytes)?;
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

pub(super) fn read_text(cancelled: &AtomicBool, limit: usize) -> Result<Vec<u8>> {
    #[cfg(not(target_os = "macos"))]
    let bytes = native::select(
        display_present,
        |backend| match backend {
            native::Backend::Wayland => wayland::read_text(cancelled, limit),
            native::Backend::X11 => x11::read_text(cancelled, limit),
        },
        || read_text_tools(cancelled, limit),
    )
    .map_err(|diagnostic| anyhow::anyhow!(diagnostic.message()))?;
    #[cfg(target_os = "macos")]
    let bytes = read_text_commands(vec![Command::new("pbpaste")], cancelled, limit)
        .map_err(|diagnostic| anyhow::anyhow!(diagnostic.message()))?;
    ensure!(!cancelled.load(Ordering::SeqCst), "clipboard read cancelled");
    ensure!(bytes.len() <= limit, TEXT_TOO_LARGE);
    std::str::from_utf8(&bytes).context("clipboard text is not UTF-8")?;
    Ok(bytes)
}

#[cfg(not(target_os = "macos"))]
fn read_text_tools(cancelled: &AtomicBool, limit: usize) -> Result<Vec<u8>, Diagnostic> {
    let mut failure = Diagnostic::NoDisplay;
    for mut probe in probes() {
        let out = match crate::exec::run_cancellable(&mut probe, TIMEOUT, 16 * 1024, cancelled) {
            Ok(out) => out,
            Err(error) => {
                failure = Diagnostic::Failed(error.to_string());
                continue;
            }
        };
        if out.cancelled || cancelled.load(Ordering::SeqCst) {
            return Err(Diagnostic::Failed("clipboard read cancelled".into()));
        }
        if out.timed_out {
            return Err(Diagnostic::Failed("clipboard read timed out".into()));
        }
        if out.truncated {
            return Err(Diagnostic::Failed("clipboard type list exceeds 16 KiB".into()));
        }
        if !out.success() {
            failure = Diagnostic::Failed(tool_error(&out));
            continue;
        }
        let wayland = probe.get_program() == "wl-paste";
        let types = String::from_utf8_lossy(&out.stdout);
        let Some(target) = tool_text_type(&types, wayland) else { return Ok(Vec::new()) };
        let mut command = Command::new(probe.get_program());
        if wayland {
            command.args(["--no-newline", "--type", target]);
        } else {
            command.args(["-selection", "clipboard", "-out", "-target", target]);
        }
        return read_text_commands(vec![command], cancelled, limit);
    }
    Err(failure)
}

#[cfg(not(target_os = "macos"))]
fn tool_text_type(types: &str, wayland: bool) -> Option<&'static str> {
    let targets = if wayland { ["text/plain;charset=utf-8", "text/plain"] } else { ["UTF8_STRING", "STRING"] };
    targets.into_iter().find(|target| types.lines().any(|line| line.trim() == *target))
}

fn read_text_commands(commands: Vec<Command>, cancelled: &AtomicBool, limit: usize) -> Result<Vec<u8>, Diagnostic> {
    let mut failure = Diagnostic::NoDisplay;
    for mut command in commands {
        let result = crate::exec::run_cancellable(&mut command, TIMEOUT, limit, cancelled);
        match result {
            Ok(out) => {
                if out.cancelled || cancelled.load(Ordering::SeqCst) {
                    return Err(Diagnostic::Failed("clipboard read cancelled".into()));
                }
                if out.truncated {
                    return Err(Diagnostic::Failed(TEXT_TOO_LARGE.into()));
                }
                if out.timed_out {
                    return Err(Diagnostic::Failed("clipboard read timed out".into()));
                }
                if out.success() {
                    #[cfg(not(target_os = "macos"))]
                    if command.get_args().last().is_some_and(|arg| arg == "STRING") {
                        return Ok(out.stdout.into_iter().map(char::from).collect::<String>().into_bytes());
                    }
                    return Ok(out.stdout);
                }
                failure = Diagnostic::Failed(tool_error(&out));
            }
            Err(error) => failure = Diagnostic::Failed(error.to_string()),
        }
    }
    Err(failure)
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
    fn text_tools_preserve_bytes_and_refuse_overflow_timeout_and_cancellation() {
        let stop = AtomicBool::new(false);
        assert_eq!(read_text_commands(vec![shell("printf 'a\\nβ'")], &stop, 4).unwrap(), "a\nβ".as_bytes());
        assert!(
            read_text_commands(vec![shell("printf 12345")], &stop, 4).unwrap_err().message().contains(TEXT_TOO_LARGE)
        );
        assert_eq!(read_text_commands(vec![shell("printf ''")], &stop, 4).unwrap(), b"");
        assert!(read_text_commands(vec![shell("sleep 60")], &stop, 4).unwrap_err().message().contains("timed out"));
        stop.store(true, Ordering::SeqCst);
        assert!(
            read_text_commands(vec![shell("printf secret")], &stop, 100).unwrap_err().message().contains("cancelled")
        );
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(
                tool_text_type("text/plain\ntext/plain;charset=utf-8\n", true),
                Some("text/plain;charset=utf-8")
            );
            assert_eq!(tool_text_type("STRING\nUTF8_STRING\n", false), Some("UTF8_STRING"));
            assert_eq!(tool_text_type("STRING\n", false), Some("STRING"));
            assert_eq!(tool_text_type("image/png\n", true), None);
            assert_eq!(tool_text_type("", false), None);
        }
    }

    #[test]
    fn control_v_is_a_local_key_not_paste_data_or_a_terminal_response() {
        for key in [
            b"\x16".as_slice(),
            b"\x1b[118;5u",
            b"\x1b[118;5:1u",
            b"\x1b[118:86:118;5:2u",
            b"\x1b[118;133u",
            b"\x1b[118;5;118u",
        ] {
            let input = [b"before".as_slice(), key, b"after"].concat();
            for split in 0..=input.len() {
                let mut parser = Keys::default();
                let mut parts = parser.feed(&input[..split]);
                parts.extend(parser.feed(&input[split..]));
                assert_eq!(parts.iter().filter(|(_, image)| *image).count(), 1, "split {split}");
                assert_eq!(parts.iter().find(|(_, image)| *image).unwrap().0, key);
                assert_eq!(parts.into_iter().flat_map(|(raw, _)| raw).collect::<Vec<_>>(), input);
            }
        }
        for input in [
            b"\x1b[200~\x16\x1b[118;5u\x1b[201~".as_slice(),
            b"\x1b]52;c;\x16\x07",
            b"\x1b[118;5:3u",
            b"\x1b[118;7u",
            b"\x1b[118;6u",
        ] {
            let mut parser = Keys::default();
            let parts: Vec<_> = input.iter().flat_map(|b| parser.feed(&[*b])).collect();
            assert!(parts.iter().all(|(_, image)| !image));
            assert_eq!(parts.into_iter().flat_map(|(raw, _)| raw).collect::<Vec<_>>(), input);
        }
        let mut parser = Keys::default();
        assert!(parser.feed(b"\x1b").is_empty());
        assert!(parser.pending());
        assert_eq!(parser.feed(&[]), [(b"\x1b".to_vec(), false)]);
        assert!(!parser.pending());
    }

    #[test]
    fn advertised_types_do_not_read_text_or_fall_back_from_a_text_clipboard() {
        let stop = AtomicBool::new(false);
        assert!(
            !advertised(vec![shell("printf 'text/plain\\nUTF8_STRING\\n'"), shell("exit 99")], &stop, TIMEOUT).unwrap()
        );
        #[cfg(not(target_os = "macos"))]
        let image = "printf 'text/plain\\nimage/png\\n'";
        #[cfg(target_os = "macos")]
        let image = "printf '«class PNGf», 128, «class utf8», 7'";
        assert!(advertised(vec![shell("exit 1"), shell(image)], &stop, TIMEOUT).unwrap());
        assert!(!advertised(vec![shell("printf text"), shell(image)], &stop, TIMEOUT).unwrap());
        assert!(matches!(advertised(Vec::new(), &stop, TIMEOUT), Err(Diagnostic::NoDisplay)));
        #[cfg(not(target_os = "macos"))]
        {
            let commands = probe_commands(|_| true);
            assert_eq!(commands[0].get_program(), "wl-paste");
            assert_eq!(commands[0].get_args().collect::<Vec<_>>(), ["--list-types"]);
            assert_eq!(commands[1].get_program(), "xclip");
            assert_eq!(
                commands[1].get_args().collect::<Vec<_>>(),
                ["-selection", "clipboard", "-out", "-target", "TARGETS"]
            );
            assert!(probe_commands(|_| false).is_empty());
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(probes()[0].get_program(), "osascript");
            assert_eq!(probes()[0].get_args().collect::<Vec<_>>(), ["-e", "clipboard info"]);
        }
    }

    #[test]
    fn probe_failures_keep_the_tool_error_and_distinguish_missing_tools_from_no_image() {
        let stop = AtomicBool::new(false);
        for (tool, package) in
            [("wl-paste", "wl-clipboard"), ("xclip", "xclip"), ("osascript", "osascript (macOS system tools)")]
        {
            let mut command = Command::new(tool);
            command.env("PATH", "/no-such-clipboard-bin");
            let error = advertised(vec![command], &stop, TIMEOUT).unwrap_err();
            assert!(matches!(error, Diagnostic::MissingTool(found) if found == package));
            assert!(error.message().ends_with(&format!("install {package}")));
        }
        let mut missing = Command::new("wl-paste");
        missing.env("PATH", "/no-such-clipboard-bin");
        let error = advertised(vec![missing, shell("echo 'owner gone' >&2; exit 1")], &stop, TIMEOUT).unwrap_err();
        assert!(matches!(error, Diagnostic::Failed(_)));
        assert!(error.message().contains("owner gone"));
        for (script, expected) in [
            ("printf 'Failed to connect to a Wayland server\\n' >&2; exit 1", "Failed to connect to a Wayland server"),
            ("exit 7", "exited with status 7"),
            ("sleep 60", "timed out"),
            ("head -c 16385 /dev/zero", "type list exceeds 16 KiB"),
        ] {
            let error =
                advertised(vec![shell(script), shell("exit 99")], &stop, Duration::from_millis(100)).unwrap_err();
            assert!(matches!(error, Diagnostic::Failed(_)));
            assert!(error.message().contains(expected), "{}", error.message());
        }
        assert!(!advertised(vec![shell("exit 1"), shell("printf text/plain")], &stop, TIMEOUT).unwrap());
        let error = run(&mut shell("echo 'permission denied' >&2; exit 1"), &stop, 4096, TIMEOUT).unwrap_err();
        assert!(error.to_string().contains("permission denied"));
    }

    #[test]
    fn notices_are_once_per_cause_wrap_in_notice_style_and_expire_without_consuming_keys() {
        let mut notices = Notices::default();
        for diagnostic in [
            Diagnostic::Empty,
            Diagnostic::MissingTool("wl-clipboard"),
            Diagnostic::Failed("wl-paste: permission denied".into()),
            Diagnostic::NoDisplay,
            Diagnostic::NotAgent,
        ] {
            let message = diagnostic.message();
            assert!(notices.show(diagnostic));
            for cols in [80, 124] {
                let bytes = String::from_utf8(notices.render(cols, 24)).unwrap();
                assert!(bytes.starts_with("\x1b7\x1b[?7l"));
                assert!(bytes.ends_with("\x1b[?7h\x1b8"));
                for line in crate::text::wrap(&message, cols as usize - 2) {
                    assert!(bytes.contains(&line));
                }
            }
            assert!(!notices.expire(std::time::Instant::now()));
            assert!(notices.expire(std::time::Instant::now() + Duration::from_secs(5)));
            assert!(notices.render(80, 24).is_empty());
        }
        assert!(!notices.show(Diagnostic::Empty));
        assert!(!notices.show(Diagnostic::MissingTool("xclip")));
        assert!(!notices.show(Diagnostic::Failed("a different error".into())));
        assert!(!notices.show(Diagnostic::NoDisplay));
        assert!(!notices.show(Diagnostic::NotAgent));
        assert!(Notices::default().show(Diagnostic::Empty));
        let mut notices = Notices::default();
        notices.show(Diagnostic::Failed("bad\x1b[31m\nerror".into()));
        assert!(!String::from_utf8(notices.render(124, 24)).unwrap().contains("bad\x1b"));
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
        assert_eq!(both[0].get_args().collect::<Vec<_>>(), ["--no-newline", "--type", "image/png"]);
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
