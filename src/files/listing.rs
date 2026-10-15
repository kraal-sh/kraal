//! Directory listings, local or over ssh, run on background threads.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use super::{Entry, Location, Remote};
use crate::text::unix_seconds;

/// How long a remote listing may take before ssh is killed.
pub(super) const SSH_TIMEOUT: Duration = Duration::from_secs(15);
/// Options inserted before the destination of every listing ssh.
const SSH_OPTIONS: [&str; 5] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-T"];
/// Precedes the remote listing, so that noise printed by login scripts is skipped.
pub(super) const MARKER: &[u8] = b"KRAAL:LIST\0";
/// The most a remote listing may print (a huge directory is cut there).
const MAX_LISTING: usize = 16 << 20;

/// The result of a listing: the resolved directory and its entries, or an error message.
pub(super) type Listing = Result<(Location, Vec<Entry>), String>;

/// How remote listings run.
#[derive(Debug, Clone)]
pub(super) struct Ssh {
    pub(super) program: String,
    pub(super) timeout: Duration,
}

impl Default for Ssh {
    fn default() -> Self {
        Ssh { program: "ssh".to_owned(), timeout: SSH_TIMEOUT }
    }
}

/// Lists `location`; runs on a background thread.
pub(super) fn list(location: &Location, ssh: &Ssh, cancel: &AtomicBool) -> Listing {
    match location {
        Location::Laptop { .. } => Err("laptop listings belong to the attached client".into()),
        Location::Local(path) => {
            let (path, entries) = list_local(path)?;
            Ok((Location::Local(path), entries))
        }
        Location::Remote(remote) => {
            let (path, entries) = list_remote(remote, ssh, cancel)?;
            Ok((Location::Remote(remote.with_path(path)), entries))
        }
    }
}

pub(super) fn list_local(path: &Path) -> Result<(PathBuf, Vec<Entry>), String> {
    let path = resolve_local(path);
    let reader = fs::read_dir(&path).map_err(|err| format!("cannot list {}: {err}", path.display()))?;
    let entries = reader
        .filter_map(Result::ok)
        .map(|entry| {
            let entry_path = entry.path();
            // Size and date are the link's own, like `ls -l` and the remote listing;
            // links are followed to tell whether they can be entered.
            let own = fs::symlink_metadata(&entry_path).ok();
            let is_link = own.as_ref().is_some_and(|m| m.file_type().is_symlink());
            let is_dir = if is_link {
                fs::metadata(&entry_path).is_ok_and(|m| m.is_dir())
            } else {
                own.as_ref().is_some_and(fs::Metadata::is_dir)
            };
            let raw = entry.file_name();
            Entry {
                name: raw.to_string_lossy().into_owned(),
                is_dir,
                is_link,
                size: own.as_ref().map_or(0, fs::Metadata::len),
                mtime: own.and_then(|m| m.modified().ok()).map_or(0, unix_seconds),
                exact: raw.to_str().is_some(),
            }
        })
        .collect();
    Ok((path, entries))
}

/// Expands `~`, makes the path absolute and removes `.` and `..` components.
pub(super) fn resolve_local(path: &Path) -> PathBuf {
    let expanded = match (path.strip_prefix("~"), home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    };
    let absolute = std::path::absolute(&expanded).unwrap_or(expanded);
    let mut normal = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other.as_os_str()),
        }
    }
    normal
}

pub(super) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|home| !home.is_empty()).map(PathBuf::from)
}

pub(super) fn list_remote(remote: &Remote, ssh: &Ssh, cancel: &AtomicBool) -> Result<(String, Vec<Entry>), String> {
    let args = ssh_command_args(remote).ok_or_else(|| format!("no ssh destination for {}", remote.server))?;
    let mut cmd = Command::new(&ssh.program);
    cmd.args(&args);
    let output = crate::exec::run_cancellable(&mut cmd, ssh.timeout, MAX_LISTING, cancel)
        .map_err(|err| format!("cannot run {}: {err}", ssh.program))?;
    if output.cancelled {
        return Err("cancelled".to_owned());
    }
    if output.timed_out {
        return Err(format!("timed out after {:?}", ssh.timeout));
    }
    // A listing cut short, by the size limit or a failure, would show entries that are not all
    // there, the last one possibly with a partial name: nothing is shown.
    if output.truncated {
        return Err(format!("too many entries to list here (over {} MiB): use a shell (key s)", MAX_LISTING >> 20));
    }
    if !output.success() {
        return Err(failure_message(&output));
    }
    parse_listing(&output.stdout, unix_seconds(SystemTime::now()))
}

/// Arguments for ssh: the user's options, ours, the destination, then the remote command.
pub(super) fn ssh_command_args(remote: &Remote) -> Option<Vec<String>> {
    let (destination, options) = remote.ssh_args.split_last()?;
    let mut args = options.to_vec();
    args.extend(SSH_OPTIONS.iter().map(|option| (*option).to_owned()));
    args.push(destination.clone());
    args.push(remote_command(&remote.path));
    Some(args)
}

/// The command ssh runs: a POSIX script, wrapped in `sh -c` so that it works
/// whatever the login shell (fish, csh, …).
pub(super) fn remote_command(path: &str) -> String {
    crate::shell::sh_c(&remote_script(path))
}

/// Prints `MARKER`, the resolved directory, then the entries. With GNU find: `F`, then
/// `type\ttarget-type\tsize\tmtime\tname` records. Elsewhere: `S`, then the same records with the
/// exact time where BSD `stat` gives it (macOS), else an `ls` date in UTC, built by the shell entry
/// by entry so that names come from the shell itself,
/// not from parsing `ls` (where `a -> b` could be a name or a link). Records end with NUL, which
/// cannot appear in names.
pub(super) fn remote_script(path: &str) -> String {
    let cd = format!("cd -- {} || exit 1", remote_dir_arg(path));
    [
        cd.as_str(),
        "printf 'KRAAL:LIST\\0%s\\0' \"$PWD\"",
        "if find . -maxdepth 0 -printf '' >/dev/null 2>&1",
        "then printf 'F\\0'",
        "find . -mindepth 1 -maxdepth 1 -printf '%y%Y\\t%s\\t%T@\\t%f\\0' || exit 1",
        "else printf 'S\\0'",
        "for f in .* *; do case $f in .|..) continue;; esac; \
         [ -e \"$f\" ] || [ -L \"$f\" ] || continue; \
         if [ -L \"$f\" ]; then k=l; elif [ -d \"$f\" ]; then k=d; else k=f; fi; \
         if [ -d \"$f\" ]; then t=d; else t=f; fi; \
         m=$(stat -f %m -- \"$f\" 2>/dev/null); \
         set -- $(LC_ALL=C TZ=UTC0 ls -ldn -- \"$f\" 2>/dev/null); \
         case $m in ''|*[!0-9]*) d=\"$6 $7 $8\";; *) d=\"@$m\";; esac; \
         printf '%s%s\\t%s\\t%s\\t%s\\0' \"$k\" \"$t\" \"$5\" \"$d\" \"$f\"; done",
        "fi",
        "exit 0",
    ]
    .join("; ")
}

/// A shell word for a remote path (a directory to list, a file to edit): `~` stays expandable,
/// the rest is quoted.
pub(crate) fn remote_dir_arg(path: &str) -> String {
    if path.is_empty() || path == "~" {
        return "\"$HOME\"".to_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", crate::shell::quote_always(rest));
    }
    if let Some(rest) = path.strip_prefix('~') {
        let (user, tail) = rest.split_once('/').unwrap_or((rest, ""));
        if !user.is_empty() && user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return format!("~{user}/{}", crate::shell::quote_always(tail));
        }
    }
    crate::shell::quote_always(path)
}

/// The last lines of stderr, or the exit status when there are none.
pub(super) fn failure_message(output: &crate::exec::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    if lines.is_empty() {
        return match output.code {
            Some(255) => "ssh failed (exit status 255)".to_owned(),
            Some(code) => format!("listing failed (exit status {code})"),
            None => "listing failed (killed by a signal)".to_owned(),
        };
    }
    lines[lines.len().saturating_sub(3)..].join("\n")
}

/// Parses the output of [`remote_script`] into the resolved directory and its entries. Only
/// records ended by their NUL count: what follows the last one is a record cut short.
pub(super) fn parse_listing(stdout: &[u8], now: i64) -> Result<(String, Vec<Entry>), String> {
    let unexpected = || "the server's answer is not a listing".to_owned();
    let start = stdout.windows(MARKER.len()).position(|window| window == MARKER).ok_or_else(unexpected)?;
    let mut fields = stdout[start + MARKER.len()..].splitn(3, |&b| b == 0);
    let raw_dir = fields.next().ok_or_else(unexpected)?;
    let mode = fields.next().ok_or_else(unexpected)?;
    let body = fields.next().unwrap_or_default();
    if raw_dir.is_empty() {
        return Err(unexpected());
    }
    // Every path is built from this one: shown approximately, it could lead elsewhere.
    let dir = std::str::from_utf8(raw_dir)
        .map_err(|_| {
            "this directory's path is not valid UTF-8: kraal cannot point at it safely; use a shell".to_owned()
        })?
        .to_owned();
    let mut records: Vec<&[u8]> = body.split(|&b| b == 0).collect();
    records.pop();
    let entries = match mode {
        b"F" => records.into_iter().filter_map(parse_find_record).collect(),
        b"S" => records.into_iter().filter_map(|r| parse_shell_record(r, now)).collect(),
        _ => return Err(unexpected()),
    };
    Ok((dir, entries))
}

/// Parses one `%y%Y\t%s\t%T@\t%f` record of GNU find.
pub(super) fn parse_find_record(record: &[u8]) -> Option<Entry> {
    let mut fields = record.splitn(4, |&b| b == b'\t');
    let (kind, size, mtime, name) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?);
    let exact = std::str::from_utf8(name).is_ok();
    let name = String::from_utf8_lossy(name).into_owned();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let own = *kind.first()?;
    let target = kind.get(1).copied().unwrap_or(own);
    let mtime = std::str::from_utf8(mtime).ok()?.split('.').next()?.parse().unwrap_or(0);
    Some(Entry {
        name,
        is_dir: target == b'd',
        is_link: own == b'l',
        size: std::str::from_utf8(size).ok().and_then(|s| s.parse().ok()).unwrap_or(0),
        mtime,
        exact,
    })
}

/// Parses one record of the shell fallback: `kt\tsize\tMon DD HH:MM|YYYY\tname`, or `@seconds`
/// in place of the date.
pub(super) fn parse_shell_record(record: &[u8], now: i64) -> Option<Entry> {
    let mut fields = record.splitn(4, |&b| b == b'\t');
    let (kind, size, date, name) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?);
    if name.is_empty() || name == b"." || name == b".." {
        return None;
    }
    let date = std::str::from_utf8(date).ok()?;
    let mut parts = date.split_whitespace();
    let mtime = match (date.strip_prefix('@'), parts.next(), parts.next(), parts.next()) {
        (Some(secs), ..) => secs.parse().unwrap_or(0),
        // `ls` ran with TZ=UTC0: its wall-clock time is UTC, whatever the server's time zone.
        (None, Some(m), Some(d), Some(t)) => ls_time(m, d, t, now).unwrap_or(0),
        _ => 0,
    };
    Some(Entry {
        name: String::from_utf8_lossy(name).into_owned(),
        is_dir: kind.get(1) == Some(&b'd'),
        is_link: kind.first() == Some(&b'l'),
        // Devices show `major,` where the size goes: no size then.
        size: std::str::from_utf8(size).ok().and_then(|s| s.parse().ok()).unwrap_or(0),
        mtime,
        exact: std::str::from_utf8(name).is_ok(),
    })
}

/// The wall-clock time of an `ls` date (`Jan  1 12:00` this year or `Jan  1  2023`), in seconds
/// as if it were UTC.
pub(super) fn ls_time(month: &str, day: &str, time: &str, now: i64) -> Option<i64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = u32::try_from(MONTHS.iter().position(|m| *m == month)? + 1).ok()?;
    // What `ls` prints, nothing that would take the arithmetic past its range.
    let day: u32 = day.parse().ok().filter(|d| (1..=31).contains(d))?;
    if let Some((hour, minute)) = time.split_once(':') {
        let hour = hour.parse::<i64>().ok().filter(|h| (0..24).contains(h))?;
        let minute = minute.parse::<i64>().ok().filter(|m| (0..60).contains(m))?;
        let clock = hour * 3600 + minute * 60;
        let (year, _, _) = civil_from_days(now.div_euclid(86_400));
        let this_year = days_from_civil(year, month, day) * 86_400 + clock;
        // Recent dates show no year: a date in the future belongs to last year.
        Some(if this_year > now.saturating_add(86_400) {
            days_from_civil(year - 1, month, day) * 86_400 + clock
        } else {
            this_year
        })
    } else {
        let year = time.parse::<i64>().ok().filter(|y| (0..=1_000_000).contains(y))?;
        Some(days_from_civil(year, month, day) * 86_400)
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
pub(super) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day count since 1970-01-01.
pub(super) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days.saturating_add(719_468);
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * shifted_month + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 }).unwrap_or(1);
    let year = year_of_era + era * 400;
    (if month <= 2 { year + 1 } else { year }, month, day)
}
