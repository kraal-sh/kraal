use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{Account, Accounts};
use crate::paths;

/// One of an account's usage windows: five hours, or a week.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// How much of the window is used, in percent.
    pub used_percent: f64,
    /// When the window starts again from zero, in Unix seconds; `None` when the source does not
    /// say.
    pub resets_at: Option<i64>,
}

/// The latest usage seen for an account, saved under the state directory ([`usage_path`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// The five-hour window, when a source reported it.
    pub five_hour: Option<Window>,
    /// The weekly window, when a source reported it.
    pub weekly: Option<Window>,
    /// Unix seconds when this was observed.
    pub observed_at: i64,
    /// Where it was read: "statusline" (Claude Code's status line), "codex" (what Codex reports)
    /// or "screen" (a limit message on the agent's screen).
    pub source: String,
}

impl Usage {
    fn windows(&self) -> impl Iterator<Item = &Window> {
        self.five_hour.iter().chain(self.weekly.iter())
    }

    /// Each valid window with the time it counts until: its reset or, when it does not say when it
    /// resets, the end of its period after it was seen.
    fn valid_windows(&self, now: i64) -> [Option<(&Window, i64)>; 2] {
        [(&self.five_hour, 5 * 3600), (&self.weekly, 7 * 24 * 3600)].map(|(window, period)| {
            window
                .as_ref()
                .map(|w| (w, w.resets_at.unwrap_or(self.observed_at.saturating_add(period))))
                .filter(|(_, end)| *end > now)
        })
    }

    /// Five-hour and weekly percentages: absent when unreported, zero when expired.
    pub fn percentages(&self, now: i64) -> impl Iterator<Item = Option<f64>> {
        [&self.five_hour, &self.weekly]
            .into_iter()
            .zip(self.valid_windows(now))
            .map(|(reported, valid)| reported.as_ref().map(|_| valid.map_or(0.0, |(w, _)| w.used_percent)))
    }

    /// Highest usage still counting, or `None` when no windows are valid.
    pub fn valid_peak(&self, now: i64) -> Option<f64> {
        self.valid_windows(now)
            .into_iter()
            .flatten()
            .fold(None, |peak, (w, _)| Some(peak.unwrap_or(0.0).max(w.used_percent)))
    }

    /// Highest usage among windows that have not reset yet.
    pub fn peak(&self, now: i64) -> f64 {
        self.valid_peak(now).unwrap_or(0.0)
    }

    /// When [`Usage::peak`] changes next with nothing new seen: the earliest reset of a window
    /// that counts at `now`.
    pub fn next_end(&self, now: i64) -> Option<i64> {
        self.valid_windows(now).into_iter().flatten().map(|(_, end)| end).min()
    }

    /// A limit message seen on the agent's screen, saying `pct` percent of a window (weekly or
    /// five-hour) is used: the latest word, up or down. A window that already reset says
    /// nothing any more; the screen does not tell when this one resets, so at the latest when
    /// its period ends (a full account is not full for ever).
    pub fn observe_screen(&mut self, weekly: bool, pct: f64, now: i64) {
        let w = if weekly { &mut self.weekly } else { &mut self.five_hour };
        let current = w.as_ref().filter(|w| w.resets_at.is_none_or(|r| r > now));
        let period = if weekly { 7 * 24 * 3600 } else { 5 * 3600 };
        let resets_at = current.and_then(|w| w.resets_at).unwrap_or(now.saturating_add(period));
        *w = Some(Window { used_percent: pct, resets_at: Some(resets_at) });
        self.observed_at = now;
        self.source = "screen".into();
    }

    /// When the fullest window resets.
    pub fn next_reset(&self, now: i64) -> Option<i64> {
        self.windows()
            .filter(|w| w.resets_at.is_some_and(|r| r > now))
            .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
            .and_then(|w| w.resets_at)
    }
}

fn usage_dir() -> PathBuf {
    paths::state_dir().join("usage")
}

/// An account's usage file in the usage directory.
pub(super) fn usage_file(agent: &str, name: &str) -> String {
    format!("{agent}-{name}.json")
}

/// Where an account's latest usage is saved: the status line command Claude runs and the server
/// write it, the server and `kraal accounts` read it, and it outlives a restart.
fn usage_path(agent: &str, id: &str) -> PathBuf {
    usage_dir().join("by-id").join(usage_file(agent, id))
}

pub fn save_usage(a: &Account, u: &Usage) -> Result<()> {
    let path = usage_path(&a.agent, &a.id);
    if let Some(d) = path.parent() {
        paths::ensure_private_dir(d)?;
    }
    paths::write_private(&path, &serde_json::to_vec(u)?)?;
    Ok(())
}

fn load_usage(dir: &Path, agent: &str, id: &str) -> Option<Usage> {
    serde_json::from_slice(&std::fs::read(dir.join("by-id").join(usage_file(agent, id))).ok()?).ok()
}

/// Latest usage per account, keyed by stable identity.
#[derive(Debug, Clone, Default)]
pub struct UsageMap(pub HashMap<String, Usage>);

impl UsageMap {
    pub fn load(accounts: &Accounts) -> UsageMap {
        Self::load_from(accounts, &usage_dir())
    }

    pub(super) fn load_from(accounts: &Accounts, dir: &Path) -> UsageMap {
        let mut usage = UsageMap::default();
        for a in accounts.known() {
            let Some(target) = accounts.by_id(&a.id) else { continue };
            if let Some(u) = load_usage(dir, &a.agent, &a.id) {
                usage.observe(&target, u);
            }
        }
        usage
    }

    fn observe(&mut self, account: &Account, usage: Usage) {
        let key = account.key();
        if self.0.get(&key).is_none_or(|old| usage.observed_at > old.observed_at) {
            self.0.insert(key, usage);
        }
    }

    pub fn get(&self, a: &Account) -> Option<&Usage> {
        self.0.get(&a.key())
    }

    pub fn peak(&self, a: &Account, now: i64) -> Option<f64> {
        self.get(a).filter(|u| u.windows().next().is_some()).map(|u| u.peak(now))
    }

    /// When the usage of an account changes next with nothing new seen: a window resets (see
    /// [`Usage::next_end`]). What shows the accounts' usage at `now` holds until then.
    pub fn next_end(&self, now: i64) -> Option<i64> {
        self.0.values().filter_map(|u| u.next_end(now)).min()
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Usage from a Claude Code status line payload (`rate_limits.five_hour` / `seven_day`).
pub fn usage_from_statusline(v: &serde_json::Value) -> Option<Usage> {
    let rl = v.get("rate_limits")?;
    let window = |name: &str| -> Option<Window> {
        let w = rl.get(name)?;
        Some(Window {
            used_percent: w.get("used_percentage")?.as_f64()?,
            resets_at: w.get("resets_at").and_then(|r| r.as_i64().or_else(|| r.as_f64().map(|f| f as i64))),
        })
    };
    let u = Usage {
        five_hour: window("five_hour"),
        weekly: window("seven_day"),
        observed_at: now(),
        source: "statusline".into(),
    };
    (u.five_hour.is_some() || u.weekly.is_some()).then_some(u)
}

/// Limit messages printed by agents, e.g. "You've hit your session limit · resets 3:45pm" or
/// "You've used 85% of your weekly limit". Returns (weekly?, percent).
pub fn limit_from_screen(screen: &str) -> Option<(bool, f64)> {
    let tail: Vec<&str> = screen.lines().rev().filter(|l| !l.trim().is_empty()).take(15).collect();
    for line in tail {
        let l = line.to_lowercase();
        let weekly = l.contains("weekly limit");
        let session = l.contains("session limit") || l.contains("5-hour limit") || l.contains("usage limit");
        if !(weekly || session) {
            continue;
        }
        if l.contains("hit your") || l.contains("limit reached") || l.contains("reached your") {
            return Some((weekly, 100.0));
        }
        if let Some(i) = l.find("used ")
            && let Some(p) = l[i + 5..].split('%').next()
            && let Ok(n) = p.trim().parse::<f64>()
        {
            return Some((weekly, n));
        }
    }
    None
}

/// How long until `resets_at`, for display: "resets in 1d2h", "resets in 3h05" or "resets in
/// 12m"; empty when it is not known.
pub fn describe_reset(resets_at: Option<i64>, now: i64) -> String {
    let Some(r) = resets_at else { return String::new() };
    // What an agent says: any number.
    let secs = r.saturating_sub(now).max(0);
    if secs >= 86_400 {
        format!("resets in {}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    } else if secs >= 3600 {
        format!("resets in {}h{:02}", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("resets in {}m", secs / 60)
    }
}
