//! Usage-aware routing (V3 amendment E2). Each agent is `ok`, `low` or `out`, from sources that
//! are documented or observed, never guessed:
//! - Claude: the status-line JSON (`rate_limits.five_hour` / `seven_day`, each with
//!   `used_percentage` and `resets_at`; documented for Pro, Max and Ultra), which `trio statusline`
//!   copies to `~/.trio/usage/claude.json`. Only interactive sessions render a status line, so
//!   the reading is as fresh as the last one; its age is shown with it.
//! - Codex: `token_count` events in its session logs (`rate_limits.primary` / `secondary`, each
//!   with `used_percent`, `window_minutes` and `resets_at`). Observed, not documented.
//! - A run that fails on a usage limit: out until the reset its message names, else for a
//!   cooldown (Codex's limit text is not documented).
//! - Spend per run: Claude's `total_cost_usd` (`--output-format json`), and Codex's
//!   `turn.completed.usage` tokens times the published per-token rate (an estimate). The room's
//!   `[budget]` caps a day.

use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A window at or above this share used makes the agent `low`.
pub const LOW_AT: f64 = 85.0;
/// How long a limit stop lasts when its message names no reset time.
const COOLDOWN_MIN: i64 = 60;
/// Session-log bytes read from the end when looking for the latest Codex reading.
const TAIL_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Window {
    /// "5h", "7d", "weekly", …
    pub label: String,
    /// Percent of the window used, 0 to 100.
    pub used: f64,
    /// When the window resets, on the ledger clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets: Option<String>,
    #[serde(skip)]
    pub resets_epoch: Option<i64>,
}

/// One reading of an agent's windows and where it came from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    pub windows: Vec<Window>,
    /// Epoch seconds when the source recorded it.
    pub observed: i64,
    pub source: &'static str,
}

/// A run that hit a usage limit: out until `until` (epoch seconds).
#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    pub until: i64,
    pub reason: String,
}

/// What the island shows and routing reads.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Usage {
    /// ok | low | out
    pub state: &'static str,
    /// When `out` ends, on the ledger clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub windows: Vec<Window>,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub source: &'static str,
    /// When the window reading was taken, on the ledger clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<String>,
    /// Dollars spent today by this room's runs (estimated for Codex).
    pub spent_today: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<f64>,
}

impl Usage {
    pub fn out(&self) -> bool {
        self.state == "out"
    }
    /// "out until 15:00 (5h window used up)", for status lines.
    pub fn why(&self) -> String {
        let until = self.until.as_deref().map(|u| format!(" until {}", hhmm(u))).unwrap_or_default();
        let reason = self.reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
        format!("{}{until}{reason}", self.state)
    }
}

/// `2026-11-24T15:00` -> `15:00`.
pub fn hhmm(at: &str) -> &str {
    at.split_once('T').map_or(at, |(_, t)| t)
}

pub fn clock(epoch: i64) -> String {
    Local.timestamp_opt(epoch, 0).single().map(|d| d.format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_default()
}

pub fn epoch_now() -> i64 {
    Local::now().timestamp()
}

/// Combine a window reading, a limit stop, today's spend and the budget into one state.
pub fn assess(reading: Option<&Reading>, stop: Option<&Stop>, spent: f64, budget: Option<f64>, now: i64) -> Usage {
    // A window whose reset has passed is empty again, whatever the stale reading says. A full
    // window that names no reset is trusted for COOLDOWN_MIN after it was read, no longer.
    let windows: Vec<Window> = reading
        .map(|r| {
            r.windows
                .iter()
                .map(|w| {
                    let ends = w.resets_epoch.or_else(|| (w.used >= 100.0).then_some(r.observed + COOLDOWN_MIN * 60));
                    if ends.is_some_and(|t| t <= now) { Window { used: 0.0, resets: None, resets_epoch: None, ..w.clone() } } else { Window { resets_epoch: ends, resets: ends.map(clock), ..w.clone() } }
                })
                .collect()
        })
        .unwrap_or_default();
    let full = windows.iter().filter(|w| w.used >= 100.0).max_by_key(|w| w.resets_epoch.unwrap_or(i64::MAX));
    let high = windows.iter().filter(|w| w.used >= LOW_AT).max_by(|a, b| a.used.total_cmp(&b.used));
    let (state, until, reason) = if let Some(s) = stop.filter(|s| s.until > now) {
        ("out", Some(s.until), Some(s.reason.clone()))
    } else if let Some(w) = full {
        ("out", w.resets_epoch, Some(format!("{} window used up", w.label)))
    } else if let Some(b) = budget.filter(|b| spent >= *b) {
        ("out", next_midnight(now), Some(format!("today's ${b:.2} budget is spent")))
    } else if let Some(w) = high {
        ("low", None, Some(format!("{} window at {:.0}%", w.label, w.used)))
    } else {
        ("ok", None, None)
    };
    Usage {
        state,
        until: until.map(clock),
        reason,
        windows,
        source: reading.map_or("", |r| r.source),
        observed: reading.map(|r| clock(r.observed)),
        spent_today: (spent * 100.0).round() / 100.0,
        budget,
    }
}

fn next_midnight(now: i64) -> Option<i64> {
    let d = Local.timestamp_opt(now, 0).single()?.date_naive().succ_opt()?;
    Local.from_local_datetime(&d.and_time(NaiveTime::MIN)).earliest().map(|t| t.timestamp())
}

// ---- Claude: the status-line copy ---------------------------------------------------------

pub fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

/// `~/.trio/usage/claude.json`, or under `TRIO_HOME` when set (as the hook store does).
pub fn claude_file() -> Option<PathBuf> {
    let trio = std::env::var_os("TRIO_HOME").map(PathBuf::from).or_else(|| Some(home()?.join(".trio")))?;
    Some(trio.join("usage").join("claude.json"))
}

/// What `trio statusline` stores: the epoch it ran and the status line's `rate_limits` as given.
#[derive(Serialize, Deserialize)]
pub struct ClaudeCopy {
    pub at: i64,
    pub rate_limits: serde_json::Value,
}

/// Read `rate_limits` from a status-line JSON object (the documented field names): the
/// subscription windows, and the spend limit a Claude apps gateway sets.
pub fn claude_windows(rate_limits: &serde_json::Value) -> Vec<Window> {
    [("five_hour", "5h"), ("seven_day", "7d"), ("spend_limit", "spend")]
        .iter()
        .filter_map(|(key, label)| {
            let w = rate_limits.get(key)?;
            let used = w.get("used_percentage").and_then(serde_json::Value::as_f64)?;
            let resets_epoch = w.get("resets_at").and_then(epoch_of);
            let label = match w.get("period").and_then(serde_json::Value::as_str) {
                Some(p) if *key == "spend_limit" => format!("{p} spend"),
                _ => label.to_string(),
            };
            Some(Window { label, used: used.clamp(0.0, 100.0), resets: resets_epoch.map(clock), resets_epoch })
        })
        .collect()
}

/// Epoch seconds from a number (seconds, or milliseconds when that large) or an RFC 3339 string.
fn epoch_of(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.as_f64() {
        let n = n as i64;
        return Some(if n > 100_000_000_000 { n / 1000 } else { n });
    }
    DateTime::parse_from_rfc3339(v.as_str()?).ok().map(|d| d.timestamp())
}

pub fn claude_reading(path: &Path) -> Option<Reading> {
    let c: ClaudeCopy = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let windows = claude_windows(&c.rate_limits);
    (!windows.is_empty()).then_some(Reading { windows, observed: c.at, source: "Claude status line" })
}

// ---- Codex: session logs ------------------------------------------------------------------

/// The newest session log under `~/.codex/sessions/YYYY/MM/DD/`, by name order then mtime. Reads
/// three directory levels newest first, so the cost does not grow with history.
pub fn codex_latest_log(sessions: &Path) -> Option<PathBuf> {
    let newest_dir = |p: &Path| -> Option<PathBuf> {
        // Only the dated YYYY / MM / DD folders: anything else would sort after the digits.
        let dated = |p: &Path| p.file_name().is_some_and(|n| !n.is_empty() && n.to_string_lossy().bytes().all(|b| b.is_ascii_digit()));
        let mut v: Vec<PathBuf> = std::fs::read_dir(p).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir() && dated(p)).collect();
        v.sort();
        v.pop()
    };
    let day = newest_dir(&newest_dir(&newest_dir(sessions)?)?)?;
    std::fs::read_dir(day)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("rollout-") && e.file_name().to_string_lossy().ends_with(".jsonl"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// The latest `token_count` reading with rate limits in a session log's tail.
pub fn codex_reading(log: &Path) -> Option<Reading> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // Skip a line cut by the tail start; read newest first.
    let lines = text.lines().skip(usize::from(start > 0));
    lines.collect::<Vec<_>>().into_iter().rev().find_map(codex_line)
}

fn codex_line(line: &str) -> Option<Reading> {
    // Only event records whose payload is a token count; chat text is never parsed.
    let mut cut = line.len().min(256);
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &line[..cut];
    if !head.contains("\"event_msg\"") || !head.contains("\"token_count\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "event_msg" || v.pointer("/payload/type")?.as_str()? != "token_count" {
        return None;
    }
    let observed = DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?).ok()?.timestamp();
    let rate = v.pointer("/payload/rate_limits")?;
    let windows: Vec<Window> = ["primary", "secondary"]
        .iter()
        .filter_map(|k| {
            let w = rate.get(k)?;
            let used = w.get("used_percent")?.as_f64()?;
            let minutes = w.get("window_minutes").and_then(serde_json::Value::as_i64);
            let resets_epoch = w.get("resets_at").and_then(epoch_of);
            Some(Window { label: minutes.map_or_else(|| k.to_string(), span), used: used.clamp(0.0, 100.0), resets: resets_epoch.map(clock), resets_epoch })
        })
        .collect();
    (!windows.is_empty()).then_some(Reading { windows, observed, source: "Codex session log" })
}

/// 300 -> "5h", 10080 -> "7d".
fn span(minutes: i64) -> String {
    match minutes {
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

// ---- Run output ---------------------------------------------------------------------------

/// What a run's stdout says, when it is structured (Claude `--output-format json`, Codex
/// `--json`). Plain text output is left as the reply, unparsed.
#[derive(Debug, Default, PartialEq)]
pub struct RunOut {
    pub structured: bool,
    pub reply: Option<String>,
    pub cost: Option<f64>,
    pub error: bool,
    /// Error text the output carried, for limit detection.
    pub error_text: String,
}

/// Dollars per million tokens: (input, cached input, output), from the published price list.
pub fn codex_rates(model: &str) -> Option<(f64, f64, f64)> {
    match model {
        "gpt-6.2-sol" => Some((2.0, 0.10, 10.0)),
        _ => None,
    }
}

/// The model a wake template names (`-m x` or `-c model="x"`).
pub fn model_of(wake: &[String]) -> Option<String> {
    wake.iter().position(|a| a == "-m" || a == "--model").and_then(|i| wake.get(i + 1)).cloned().or_else(|| {
        wake.iter().find_map(|a| a.strip_prefix("model=").map(|m| m.trim_matches('"').to_string()))
    })
}

pub fn parse_output(stdout: &str, rates: Option<(f64, f64, f64)>) -> RunOut {
    let t = stdout.trim();
    // Antigravity (`agy -p --output-format json`): one object with `status` and `response`.
    if t.starts_with('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(t)
        && v.get("conversation_id").is_some()
        && let Some(status) = v.get("status").and_then(|x| x.as_str())
    {
        let error = status != "SUCCESS";
        let response = v.get("response").and_then(|x| x.as_str()).map(str::to_string);
        let error_text = if error {
            let detail = v.get("error").map(|e| e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())).unwrap_or_default();
            format!("{status} {detail} {}", response.clone().unwrap_or_default())
        } else {
            String::new()
        };
        return RunOut { structured: true, reply: response.filter(|r| !error && !r.trim().is_empty()), cost: None, error, error_text };
    }
    // Claude: one JSON object (JSON lines fail to parse as one value and fall through).
    if t.starts_with('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(t)
        && (v.get("type").and_then(|x| x.as_str()) == Some("result") || v.get("result").is_some())
    {
        let error = v.get("is_error").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let result = v.get("result").and_then(|x| x.as_str()).map(str::to_string);
        return RunOut {
            structured: true,
            // An error payload may carry `errors: [..]` instead of a `result` string.
            error_text: if error {
                result.clone().filter(|r| !r.trim().is_empty()).unwrap_or_else(|| {
                    let errs = v.get("errors").and_then(|e| e.as_array()).map(|a| a.iter().map(|e| e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())).collect::<Vec<_>>().join("; "));
                    errs.unwrap_or_default()
                })
            } else {
                String::new()
            },
            reply: result.filter(|_| !error),
            cost: v.get("total_cost_usd").and_then(serde_json::Value::as_f64),
            error,
        };
    }
    // Codex: JSON lines with typed events.
    let mut out = RunOut::default();
    let (mut input, mut cached, mut output) = (0u64, 0u64, 0u64);
    for line in t.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        let Some(kind) = v.get("type").and_then(|x| x.as_str()) else { continue };
        match kind {
            "thread.started" | "turn.started" | "item.started" | "item.updated" => out.structured = true,
            "item.completed" => {
                out.structured = true;
                if v.pointer("/item/type").and_then(|x| x.as_str()) == Some("agent_message") {
                    out.reply = v.pointer("/item/text").and_then(|x| x.as_str()).map(str::to_string);
                }
            }
            "turn.completed" => {
                out.structured = true;
                let n = |k: &str| v.pointer(&format!("/usage/{k}")).and_then(serde_json::Value::as_u64).unwrap_or(0);
                input += n("input_tokens");
                cached += n("cached_input_tokens");
                output += n("output_tokens");
            }
            "turn.failed" | "error" => {
                out.structured = true;
                out.error = true;
                let msg = v.pointer("/error/message").or_else(|| v.get("message")).and_then(|x| x.as_str()).unwrap_or("");
                out.error_text.push_str(msg);
                out.error_text.push('\n');
            }
            _ => {}
        }
    }
    if out.structured && input + output > 0 {
        out.cost = rates.map(|(i, c, o)| ((input.saturating_sub(cached)) as f64 * i + cached as f64 * c + output as f64 * o) / 1_000_000.0);
    }
    if out.error {
        out.reply = None;
    }
    out
}

/// A usage-limit stop from a failed run's text, if it names one. The reset time is read from
/// "resets 3:45pm" / "try again at 15:00" (optionally after a weekday); without one the stop
/// lasts COOLDOWN_MIN minutes.
pub fn limit_stop(text: &str, now: DateTime<Local>) -> Option<Stop> {
    let lower = text.to_ascii_lowercase();
    let hit = lower.contains("usage limit") || (lower.contains("hit your") && lower.contains("limit")) || lower.contains("limit reached") || lower.contains("rate_limit_exceeded") || lower.contains("rate limit exceeded") || lower.contains("quota exceeded") || lower.contains("resource_exhausted") || lower.contains("resource has been exhausted");
    if !hit {
        return None;
    }
    let line = text.lines().find(|l| l.to_ascii_lowercase().contains("limit")).unwrap_or(text).trim();
    let reason: String = line.chars().take(100).collect();
    let reset = ["resets at ", "resets ", "try again at "].iter().find_map(|k| lower.find(k).and_then(|i| reset_time(&lower[i + k.len()..], now)));
    let until = reset.map(|t| t.timestamp()).unwrap_or(now.timestamp() + COOLDOWN_MIN * 60);
    Some(Stop { until, reason: if reset.is_some() { reason } else { format!("{reason} (no reset time given; retry in {COOLDOWN_MIN} min)") } })
}

/// "3:45pm", "3pm", "15:00", "3:45 pm", "mon 9am" -> the next such local time after `now`.
/// "nov 25th, 2026 12:05 am" (how Codex names a reset on another day) -> that time.
fn reset_time(s: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let mut words = s.split_whitespace();
    let mut w = words.next()?.trim_end_matches([',', '.', ')']);
    let day = w.parse::<chrono::Weekday>().ok();
    let mut on: Option<chrono::NaiveDate> = None;
    if day.is_some() {
        w = words.next()?.trim_end_matches([',', '.', ')']);
    } else if let Ok(month) = w.parse::<chrono::Month>() {
        let d = words.next()?.trim_end_matches(',').trim_end_matches(|c: char| c.is_ascii_alphabetic()).parse::<u32>().ok()?;
        w = words.next()?.trim_end_matches([',', '.', ')']);
        // "Dec 1 2026 9am", or with no year "Dec 1, 9am": this year, else next if it has passed.
        let year = w.parse::<i32>().ok().filter(|y| *y > 1000);
        if year.is_some() {
            w = words.next()?.trim_end_matches([',', '.', ')']);
        }
        let y = year.unwrap_or(now.year());
        let mut date = chrono::NaiveDate::from_ymd_opt(y, month.number_from_month(), d)?;
        if year.is_none() && date < now.date_naive() {
            date = chrono::NaiveDate::from_ymd_opt(y + 1, month.number_from_month(), d)?;
        }
        on = Some(date);
    }
    if w == "at" {
        w = words.next()?.trim_end_matches([',', '.', ')']);
    }
    let mut t = w.to_string();
    if !t.ends_with("am") && !t.ends_with("pm")
        && let Some(n) = words.next().map(|n| n.trim_end_matches([',', '.', ')']))
        && (n == "am" || n == "pm")
    {
        t.push_str(n);
    }
    let (num, pm) = match (t.strip_suffix("am"), t.strip_suffix("pm")) {
        (Some(x), _) => (x, Some(false)),
        (_, Some(x)) => (x, Some(true)),
        _ => (t.as_str(), None),
    };
    // A bare number ("resets 5") is not a time.
    if pm.is_none() && !num.contains(':') {
        return None;
    }
    let (h, m) = match num.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (num.parse::<u32>().ok()?, 0),
    };
    let h = match pm {
        Some(true) if h < 12 => h + 12,
        Some(false) if h == 12 => 0,
        _ => h,
    };
    let time = NaiveTime::from_hms_opt(h, m, 0)?;
    if let Some(date) = on {
        return Local.from_local_datetime(&date.and_time(time)).earliest();
    }
    let mut date = now.date_naive();
    if let Some(d) = day {
        while date.weekday() != d {
            date = date.succ_opt()?;
        }
    }
    let mut at = Local.from_local_datetime(&date.and_time(time)).earliest()?;
    if at <= now {
        at += Duration::days(if day.is_some() { 7 } else { 1 });
    }
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 24, h, m, 0).single().unwrap()
    }

    #[test]
    fn limit_messages_give_a_stop_until_the_named_reset() {
        let now = at(13, 0);
        let s = limit_stop("You've hit your session limit \u{b7} resets 3:45pm", now).unwrap();
        assert_eq!(clock(s.until), "2026-11-24T15:45");
        let s = limit_stop("Error: usage limit reached. Try again at 9:05 AM.", now).unwrap();
        assert_eq!(clock(s.until), "2026-11-25T09:05", "already past today: tomorrow");
        let s = limit_stop("You've hit your weekly limit \u{b7} resets Mon 9am", now).unwrap();
        assert_eq!(clock(s.until), "2026-11-30T09:00");
        let late = Local.with_ymd_and_hms(2026, 11, 24, 23, 58, 0).single().unwrap();
        let s = limit_stop("You\u{2019}ve hit your usage limit. Upgrade to Pro, or try again at Nov 25th, 2026 12:05 AM.", late).unwrap();
        assert_eq!(clock(s.until), "2026-11-25T00:05", "a reset on another day names its date");
        let s = limit_stop("You\u{2019}ve hit your usage limit. Try again at Dec 1st, 2026 9:30 AM.", now).unwrap();
        assert_eq!(clock(s.until), "2026-12-01T09:30");
        let s = limit_stop("You've hit your weekly limit \u{b7} resets Dec 1, 9am", now).unwrap();
        assert_eq!(clock(s.until), "2026-12-01T09:00", "no year: this year");
        let s = limit_stop("Usage limit reached; resets Nov 25 at 2pm", now).unwrap();
        assert_eq!(clock(s.until), "2026-11-25T14:00", "'at' before the time");
        let s = limit_stop("Usage limit reached; resets Jan 3, 9am", now).unwrap();
        assert_eq!(clock(s.until), "2027-01-03T09:00", "a date already past this year is next year's");
        assert!(limit_stop("429: Rate limit exceeded", now).is_some());
        let s = limit_stop("stream error: usage limit exceeded", now).unwrap();
        assert_eq!(s.until, now.timestamp() + 3600, "no time named: a cooldown");
        assert!(s.reason.contains("retry in 60 min"));
        assert_eq!(limit_stop("tests failed: 3 of 70", now), None);
    }

    #[test]
    fn windows_decide_ok_low_and_out() {
        let now = at(13, 0).timestamp();
        let w = |used: f64, resets: i64| Window { label: "5h".into(), used, resets: Some(clock(resets)), resets_epoch: Some(resets) };
        let r = |used, resets| Reading { windows: vec![w(used, resets)], observed: now - 60, source: "Claude status line" };
        assert_eq!(assess(Some(&r(40.0, now + 3600)), None, 0.0, None, now).state, "ok");
        let low = assess(Some(&r(90.0, now + 3600)), None, 0.0, None, now);
        assert_eq!((low.state, low.reason.as_deref()), ("low", Some("5h window at 90%")));
        let out = assess(Some(&r(100.0, now + 3600)), None, 0.0, None, now);
        assert_eq!((out.state, out.until.as_deref()), ("out", Some("2026-11-24T14:00")));
        assert_eq!(assess(Some(&r(100.0, now - 60)), None, 0.0, None, now).state, "ok", "the window has reset since the reading");
        let stop = Stop { until: now + 600, reason: "session limit".into() };
        assert_eq!(assess(None, Some(&stop), 0.0, None, now).why(), "out until 13:10 (session limit)");
        let no_reset = Reading { windows: vec![Window { label: "5h".into(), used: 100.0, resets: None, resets_epoch: None }], observed: now - 60, source: "Claude status line" };
        assert_eq!(assess(Some(&no_reset), None, 0.0, None, now).state, "out");
        assert_eq!(assess(Some(&no_reset), None, 0.0, None, now + 3600).state, "ok", "a full window with no reset expires");
        let broke = assess(None, None, 5.2, Some(5.0), now);
        assert_eq!((broke.state, broke.until.as_deref()), ("out", Some("2026-11-25T00:00")));
    }

    #[test]
    fn claude_status_line_fields_are_read_as_documented() {
        let v = serde_json::json!({ "five_hour": { "used_percentage": 42.5, "resets_at": "2026-11-24T15:00:00Z" }, "seven_day": { "used_percentage": 18, "resets_at": 1795600000 } });
        let w = claude_windows(&v);
        assert_eq!(w.iter().map(|w| (w.label.as_str(), w.used)).collect::<Vec<_>>(), vec![("5h", 42.5), ("7d", 18.0)]);
        assert_eq!(w[0].resets_epoch, Some(DateTime::parse_from_rfc3339("2026-11-24T15:00:00Z").unwrap().timestamp()));
        assert_eq!(w[1].resets_epoch, Some(1_795_600_000));
        let gateway = serde_json::json!({ "spend_limit": { "used_percentage": 104.0, "resets_at": 1795600000, "period": "monthly" } });
        let w = claude_windows(&gateway);
        assert_eq!((w[0].label.as_str(), w[0].used), ("monthly spend", 100.0), "a gateway spend limit is a window too");
    }

    #[test]
    fn codex_session_logs_yield_the_latest_window_reading() {
        let line = |used: f64| format!(r#"{{"timestamp":"2026-11-24T12:00:00.000Z","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"primary":{{"used_percent":{used},"window_minutes":300,"resets_at":1795600000}},"secondary":{{"used_percent":12.0,"window_minutes":10080,"resets_at":1796000000}}}}}}}}"#);
        let dir = std::env::temp_dir().join(format!("trio-usage-{}", std::process::id()));
        let day = dir.join("2026").join("11").join("24");
        std::fs::create_dir_all(&day).unwrap();
        let log = day.join("rollout-2026-11-24T12-00-00-abc.jsonl");
        let chat = r#"{"type":"response_item","payload":{"text":"\"token_count\" in a message"}}"#;
        // A multibyte character across byte 256 of a line must not stop the read.
        let wide = format!(r#"{{"type":"response_item","payload":{{"text":"{}"}}}}"#, "\u{e9}".repeat(200));
        std::fs::write(&log, format!("{}\n{}\n{chat}\n{wide}\n", line(30.0), line(55.0))).unwrap();
        assert_eq!(codex_latest_log(&dir).as_deref(), Some(log.as_path()));
        let r = codex_reading(&log).unwrap();
        assert_eq!(r.windows.iter().map(|w| (w.label.as_str(), w.used)).collect::<Vec<_>>(), vec![("5h", 55.0), ("7d", 12.0)]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn structured_run_output_gives_reply_cost_and_errors() {
        let claude = r#"{"type":"result","subtype":"success","is_error":false,"result":"XF1 passes.","total_cost_usd":0.0712}"#;
        let o = parse_output(claude, None);
        assert_eq!((o.reply.as_deref(), o.cost, o.error), (Some("XF1 passes."), Some(0.0712), false));
        let limited = r#"{"type":"result","is_error":true,"result":"You've hit your session limit · resets 3:45pm"}"#;
        let o = parse_output(limited, None);
        assert!(o.error && o.reply.is_none() && o.error_text.contains("session limit"));
        let codex = "{\"type\":\"thread.started\",\"thread_id\":\"t\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Looks right.\"}}\n{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1000000,\"cached_input_tokens\":500000,\"output_tokens\":100000}}\n";
        let o = parse_output(codex, codex_rates("gpt-6.2-sol"));
        assert_eq!(o.reply.as_deref(), Some("Looks right."));
        assert!((o.cost.unwrap() - (0.5 * 2.0 + 0.5 * 0.10 + 0.1 * 10.0)).abs() < 1e-9);
        let plain = parse_output("ready", None);
        assert!(!plain.structured && plain.reply.is_none());
        assert_eq!(model_of(&["@codex".into(), "exec".into(), "-m".into(), "gpt-6.2-sol".into()]).as_deref(), Some("gpt-6.2-sol"));
    }
}
