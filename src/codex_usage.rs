//! Credential-free usage discovery through Codex and bounded session tails.
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::accounts::now;

#[derive(Debug, Clone)]
pub struct Window {
    pub used_percent: f64,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}
#[derive(Debug, Clone, Default)]
pub struct RateLimits {
    /// The window of about five hours, if reported.
    pub five_hour: Option<Window>,
    /// The weekly window, if reported.
    pub weekly: Option<Window>,
    /// Unix seconds when observed.
    pub observed_at: i64,
}
fn field<'a>(v: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|n| v.get(n).filter(|v| !v.is_null()))
}
fn timestamp(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| rfc3339(v.as_str()?))
}
/// Strict calendar validation, fractional seconds discarded, numeric offsets supported.
fn rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let number = |a: usize, z: usize| -> Option<i64> {
        let digits = b.get(a..z)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        digits.iter().try_fold(0i64, |n, d| n.checked_mul(10)?.checked_add((d - b'0') as i64))
    };
    let (y, m, d, h, min, sec) =
        (number(0, 4)?, number(5, 7)?, number(8, 10)?, number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&m) || d < 1 || d > days[(m - 1) as usize] || h > 23 || min > 59 || sec > 59 {
        return None;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if start == i {
            return None;
        }
    }
    let offset = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if i + 6 == b.len() && b[i + 3] == b':' => {
            let oh = number(i + 1, i + 3)?;
            let om = number(i + 4, i + 6)?;
            if oh > 23 || om > 59 {
                return None;
            }
            (oh * 3600 + om * 60) * if *sign == b'+' { 1 } else { -1 }
        }
        _ => return None,
    };
    // Gregorian civil date to days since 1970-01-01 (including years before 1970).
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yo = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let day = era * 146097 + yo * 365 + yo / 4 - yo / 100 + doy - 719468;
    Some(day * 86400 + h * 3600 + min * 60 + sec - offset)
}
fn snapshot(v: &Value) -> Option<RateLimits> {
    if !v.is_object() {
        return None;
    }
    let mut out = RateLimits { observed_at: now(), ..Default::default() };
    for key in ["primary", "secondary"] {
        let Some(w) = v.get(key) else { continue };
        let Some(used_percent) = field(w, &["usedPercent", "used_percent"]).and_then(Value::as_f64) else {
            continue;
        };
        let window_minutes =
            field(w, &["windowDurationMins", "window_duration_mins", "window_minutes"]).and_then(Value::as_u64);
        let window = Window { used_percent, resets_at: field(w, &["resetsAt", "resets_at"]).and_then(timestamp) };
        match window_minutes {
            Some(1..=1440) => out.five_hour = Some(window),
            Some(7200..) => out.weekly = Some(window),
            _ => {}
        }
    }
    Some(out)
}
/// Parse only the result value; unrelated account fields are never retained.
pub fn parse_rpc_result(v: &Value) -> Option<RateLimits> {
    field(v, &["rateLimits", "rate_limits"])
        .or_else(|| field(v, &["rateLimitsByLimitId", "rate_limits_by_limit_id"])?.get("codex"))
        .and_then(snapshot)
}
/// Ask Codex via stdio. All error messages are fixed strings to avoid disclosing server data.
/// Everything happens on the calling thread, each read waiting at most until the deadline:
/// nothing is left blocked behind when Codex, or something it started, keeps the pipe open.
pub fn fetch(codex_bin: &str, home: &Path, timeout: Duration) -> anyhow::Result<RateLimits> {
    let deadline = std::time::Instant::now() + timeout;
    // In a process group of its own: when it does not answer in time, whatever it started
    // (`codex` may be a wrapper) goes with it. On Linux it runs under `kraal supervise` (this
    // very program, even once a newer one replaced it on disk), which also ends what it left
    // behind however it left (see [`crate::exec::supervise`]). Unit tests are another program:
    // they talk to Codex directly.
    // Whatever it starts carries this tag, so that even a daemon that left its tree goes too.
    let tag = crate::accounts::new_uuid();
    let spawn = |supervised: bool| {
        let mut command = if supervised {
            let mut c = Command::new("/proc/self/exe");
            c.arg("supervise").arg(codex_bin);
            c
        } else {
            Command::new(codex_bin)
        };
        crate::exec::own_group(&mut command)
            .args(["app-server", "--listen", "stdio://"])
            .env("CODEX_HOME", home)
            .env(crate::exec::TAG, &tag)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    let mut supervised = cfg!(all(target_os = "linux", not(test)));
    let mut child = match spawn(supervised) {
        Ok(child) => child,
        Err(_) if supervised => {
            supervised = false;
            spawn(false).map_err(|_| anyhow::anyhow!("could not start Codex app-server"))?
        }
        Err(_) => anyhow::bail!("could not start Codex app-server"),
    };
    let mut input = child.stdin.take().unwrap();
    let mut output = Lines { pipe: child.stdout.take().unwrap(), buf: Vec::new(), deadline };
    let result = (|| -> anyhow::Result<RateLimits> {
        let mut send = |v: Value| -> anyhow::Result<()> {
            writeln!(input, "{v}")
                .and_then(|_| input.flush())
                .map_err(|_| anyhow::anyhow!("Codex protocol write failed"))
        };
        send(
            json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"kraal","version":env!("CARGO_PKG_VERSION")}}}),
        )?;
        for id in [1, 2] {
            loop {
                let line = output.next()?;
                let v: Value =
                    serde_json::from_slice(&line).map_err(|_| anyhow::anyhow!("invalid Codex protocol JSON"))?;
                if v.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                anyhow::ensure!(v.get("error").is_none(), "Codex rate limit request failed");
                if id == 2 {
                    return parse_rpc_result(&v["result"]).ok_or_else(|| anyhow::anyhow!("missing Codex rate limits"));
                }
                break;
            }
            send(json!({"method":"initialized"}))?;
            send(json!({"id":2,"method":"account/rateLimits/read","params":null}))?;
        }
        unreachable!()
    })();
    // The supervisor ends everything below it (a moment to do so); then, and without one, a
    // wrapper's helpers that left its group (`setsid`) or its tree (a daemon) go too.
    if supervised {
        // SAFETY: plain signal to our own child, not reaped yet.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        let start = std::time::Instant::now();
        while matches!(child.try_wait(), Ok(None)) && start.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    crate::exec::kill_tree(&mut child);
    crate::exec::kill_tagged(&tag);
    result
}

/// Lines from Codex's stdout, each read waiting at most until `deadline`.
struct Lines {
    pipe: std::process::ChildStdout,
    buf: Vec<u8>,
    deadline: std::time::Instant,
}

impl Lines {
    const MAX: usize = 1_048_576;

    fn next(&mut self) -> anyhow::Result<Vec<u8>> {
        use std::os::unix::io::AsRawFd;
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                return Ok(self.buf.drain(..=i).collect());
            }
            anyhow::ensure!(self.buf.len() <= Self::MAX, "invalid Codex protocol frame");
            let left = self.deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(!left.is_zero(), "Codex app-server timed out");
            let mut fd = libc::pollfd { fd: self.pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let ms = left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
            // SAFETY: one valid pollfd, owned by `self.pipe` for the duration of the call.
            match unsafe { libc::poll(&mut fd, 1, ms) } {
                0 => anyhow::bail!("Codex app-server timed out"),
                n if n < 0 => {
                    anyhow::ensure!(
                        std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted,
                        "Codex protocol read failed"
                    );
                }
                _ => {
                    // Readable: this read does not block.
                    let mut chunk = [0u8; 64 << 10];
                    let n = self.pipe.read(&mut chunk).map_err(|_| anyhow::anyhow!("Codex protocol read failed"))?;
                    anyhow::ensure!(n > 0, "invalid Codex protocol frame");
                    self.buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }
}
/// Read at most five newest regular rollout files, with at most 256 KiB per file.
/// Symlinks are skipped, including the sessions root, so traversal stays under sessions.
pub fn from_rollouts(home: &Path) -> Option<RateLimits> {
    fn visit(path: &Path, depth: usize, files: &mut Vec<(SystemTime, PathBuf)>) {
        if depth > 16 || !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            return;
        }
        let Ok(entries) = fs::read_dir(path) else { return };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                visit(&entry.path(), depth + 1, files);
            } else if kind.is_file()
                && entry.file_name().to_str().is_some_and(|s| s.starts_with("rollout-") && s.ends_with(".jsonl"))
                && let Ok(m) = entry.metadata()
            {
                files.push((m.modified().unwrap_or(UNIX_EPOCH), entry.path()));
                files.sort_by(|a, b| b.cmp(a));
                files.truncate(5);
            }
        }
    }
    let mut files = Vec::new();
    visit(&home.join("sessions"), 0, &mut files);
    let mut best: Option<RateLimits> = None;
    for (_, path) in files {
        let Ok(mut file) = File::open(path) else { continue };
        let Ok(meta) = file.metadata() else { continue };
        let start = meta.len().saturating_sub(262_144);
        if file.seek(SeekFrom::Start(start)).is_err() {
            continue;
        }
        let mut bytes = Vec::new();
        if file.take(262_144).read_to_end(&mut bytes).is_err() {
            continue;
        }
        let mut lines = bytes.split(|b| *b == b'\n');
        if start > 0 {
            lines.next();
        }
        for line in lines {
            let Ok(v) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if v["type"] != "event_msg" || v["payload"]["type"] != "token_count" {
                continue;
            }
            let Some(mut limits) = snapshot(&v["payload"]["rate_limits"]) else {
                continue;
            };
            let Some(at) = v.get("timestamp").and_then(timestamp) else {
                continue;
            };
            limits.observed_at = at;
            if best.as_ref().is_none_or(|b| at > b.observed_at) {
                best = Some(limits);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::current_dir().unwrap().join(format!(
                "test-tmp-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn rpc_schema_and_classification() {
        let v = json!({"accountId":"ignored", "rateLimits":{"planType":"plus", "primary":{"usedPercent":80,"windowDurationMins":10080,"resetsAt":1700000000},"secondary":{"usedPercent":12,"windowDurationMins":300,"resetsAt":"1970-01-01T01:00:00+01:00"}},"rateLimitsByLimitId":null});
        let r = parse_rpc_result(&v).unwrap();
        assert_eq!(r.five_hour.as_ref().unwrap().used_percent, 12.0);
        assert_eq!(r.five_hour.unwrap().resets_at, Some(0));
        assert_eq!(r.weekly.unwrap().used_percent, 80.0);
        assert!(parse_rpc_result(&json!({})).is_none());
        for (duration, short, long) in
            [(0, false, false), (1440, true, false), (1441, false, false), (7199, false, false), (7200, false, true)]
        {
            let r =
                parse_rpc_result(&json!({"rate_limits":{"primary":{"used_percent":1.5,"window_minutes":duration}}}))
                    .unwrap();
            assert_eq!(r.five_hour.is_some(), short);
            assert_eq!(r.weekly.is_some(), long);
        }
        let r = parse_rpc_result(
            &json!({"rateLimits":{"primary":null,"secondary":{"usedPercent":1,"windowDurationMins":null}}}),
        )
        .unwrap();
        assert!(r.five_hour.is_none() && r.weekly.is_none());
        assert!(parse_rpc_result(&json!({"rateLimitsByLimitId":{"codex":{"planType":"pro"}}})).is_some());
    }
    #[test]
    fn dates() {
        for (s, expected) in [
            ("1970-01-01T00:00:00Z", 0),
            ("1970-01-01T00:00:00.123Z", 0),
            ("1970-01-01T01:30:00+01:30", 0),
            ("1969-12-31T23:00:00-01:00", 0),
            ("2000-02-29T00:00:00Z", 951782400),
        ] {
            assert_eq!(rfc3339(s), Some(expected));
        }
        for s in [
            "2023-02-29T00:00:00Z",
            "2000-13-01T00:00:00Z",
            "2000-01-00T00:00:00Z",
            "2000-01-01T24:00:00Z",
            "1970-01-01T00:00:00+24:00",
            "1970-01-01T00:00:00.Z",
            "1970-01-01T00:00:00Zgarbage",
            "é",
        ] {
            assert_eq!(rfc3339(s), None);
        }
    }
    fn record(at: &str, percent: u64) -> String {
        json!({"timestamp":at,"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":percent,"window_minutes":300,"resets_at":100}}}}).to_string()
    }
    #[test]
    fn rollout_days_and_malformed_lines() {
        let tmp = Temp::new();
        assert!(from_rollouts(&tmp.0).is_none());
        for (day, percent) in [(1, 10), (2, 20), (3, 30)] {
            let dir = tmp.0.join(format!("sessions/2026/10/{day:02}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("rollout-{day}.jsonl")),
                format!("bad json\n{}\n{{broken\n", record(&format!("2026-10-{day:02}T00:00:00Z"), percent)),
            )
            .unwrap();
        }
        let r = from_rollouts(&tmp.0).unwrap();
        assert_eq!(r.five_hour.unwrap().used_percent, 30.0);
        assert_eq!(r.observed_at, rfc3339("2026-10-03T00:00:00Z").unwrap());
    }
    #[test]
    fn rollout_tail_and_newest_file_budget() {
        let tmp = Temp::new();
        let dir = tmp.0.join("sessions");
        fs::create_dir(&dir).unwrap();
        for n in 0..6 {
            let path = dir.join(format!("rollout-{n}.jsonl"));
            let mut f = File::create(&path).unwrap();
            writeln!(f, "{}", record("2099-01-01T00:00:00Z", 99)).unwrap();
            f.write_all(&vec![b'x'; 262_144]).unwrap();
            writeln!(f, "\n{}", record(if n == 0 { "2100-01-01T00:00:00Z" } else { "2026-01-01T00:00:00Z" }, n))
                .unwrap();
            f.set_modified(UNIX_EPOCH + Duration::from_secs(n)).unwrap();
        }
        assert_eq!(from_rollouts(&tmp.0).unwrap().five_hour.unwrap().used_percent, 5.0);
    }
    fn script(tmp: &Temp, body: &str) -> PathBuf {
        let path = tmp.0.join("fake-codex");
        crate::testing::write_script(&path, &format!("#!/bin/sh\n{body}\n"));
        path
    }
    #[test]
    fn fetch_protocol() {
        let tmp = Temp::new();
        let path = script(
            &tmp,
            &r#"
[ "$1 $2 $3" = 'app-server --listen stdio://' ] || exit 1
[ -d "$CODEX_HOME" ] || exit 2
IFS= read -r line
[ "$line" = '{"id":1,"method":"initialize","params":{"clientInfo":{"name":"kraal","version":"VERSION"}}}' ] || exit 3
printf '%s\n' '{"method":"notice"}' '{"id":99,"result":{}}' '{"id":1,"result":{}}'
IFS= read -r line
[ "$line" = '{"method":"initialized"}' ] || exit 4
IFS= read -r line
[ "$line" = '{"id":2,"method":"account/rateLimits/read","params":null}' ] || exit 5
printf '%s\n' '{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":42,"windowDurationMins":300}}}}'
"#
            .replace("VERSION", env!("CARGO_PKG_VERSION")),
        );
        let r = fetch(path.to_str().unwrap(), &tmp.0, Duration::from_secs(2)).unwrap();
        assert_eq!(r.five_hour.unwrap().used_percent, 42.0);
    }
    #[test]
    fn fetch_timeout_and_redacted_errors() {
        let tmp = Temp::new();
        let path = script(&tmp, "while :; do :; done");
        let start = std::time::Instant::now();
        assert!(
            fetch(path.to_str().unwrap(), &tmp.0, Duration::from_millis(50))
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        // A wrapper's own child, holding the pipes, goes with it.
        let path = script(&tmp, "sleep 30 &\necho $! > \"$CODEX_HOME/child\"\nwait");
        assert!(fetch(path.to_str().unwrap(), &tmp.0, Duration::from_millis(500)).is_err());
        let pid: i32 = std::fs::read_to_string(tmp.0.join("child")).unwrap().trim().parse().unwrap();
        let gone = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            !crate::process::running(pid)
        });
        assert!(gone, "the wrapper's child still runs");
        let path = script(
            &tmp,
            r#"IFS= read -r line
printf '%s\n' '{"id":1,"error":{"message":"secret@example.com token123"}}'"#,
        );
        let err = fetch(path.to_str().unwrap(), &tmp.0, Duration::from_secs(2)).unwrap_err().to_string();
        assert!(!err.contains("secret") && !err.contains("token123"));
        let path = script(&tmp, "exit 0");
        assert!(fetch(path.to_str().unwrap(), &tmp.0, Duration::from_secs(2)).is_err());
    }
}
