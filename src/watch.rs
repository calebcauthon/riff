//! `riff watch` — follow the global event bus.
//!
//! A viewer only: it reads `$RIFF_ROOT/events.jsonl`, never writes to it and
//! never runs anything in response to an event.

use crate::cli::{Cli, WatchArgs};
use crate::error::{app_error, AppError};
use crate::events::RESERVED_KEYS;
use crate::models::SessionState;
use crate::paths::{active_state_file, ensure_dirs, event_bus_file, event_bus_rotated_file};
use crate::{print_out, read_json};
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::fs::{self, File};
use std::io::IsTerminal;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

struct Filters {
    types: Vec<String>,
    commands: Vec<String>,
    session: Option<String>,
    grep: Option<String>,
    since: Option<DateTime<Utc>>,
}

impl Filters {
    fn matches(&self, event: &Value, raw: &str) -> bool {
        if !self.types.is_empty() {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !self.types.iter().any(|t| t == event_type) {
                return false;
            }
        }
        if !self.commands.is_empty() {
            let command = event.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if !self.commands.iter().any(|c| c == command) {
                return false;
            }
        }
        if let Some(session) = &self.session {
            let event_session = event.get("session_id").and_then(|v| v.as_str());
            if event_session != Some(session.as_str()) {
                return false;
            }
        }
        if let Some(needle) = &self.grep {
            if !raw.to_lowercase().contains(&needle.to_lowercase()) {
                return false;
            }
        }
        if let Some(cutoff) = self.since {
            let Some(ts) = event.get("ts").and_then(|v| v.as_str()) else {
                return false;
            };
            let Ok(parsed) = DateTime::parse_from_rfc3339(ts) else {
                return false;
            };
            if parsed.with_timezone(&Utc) < cutoff {
                return false;
            }
        }
        true
    }
}

/// Parse `30s`, `10m`, `2h`, `1d`, or a bare number of seconds.
pub(crate) fn parse_since(raw: &str) -> Option<Duration> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (digits, multiplier) = match trimmed.chars().last() {
        Some('s') | Some('S') => (&trimmed[..trimmed.len() - 1], 1.0),
        Some('m') | Some('M') => (&trimmed[..trimmed.len() - 1], 60.0),
        Some('h') | Some('H') => (&trimmed[..trimmed.len() - 1], 3600.0),
        Some('d') | Some('D') => (&trimmed[..trimmed.len() - 1], 86_400.0),
        _ => (trimmed, 1.0),
    };
    let value: f64 = digits.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(value * multiplier))
}

pub(crate) fn cmd_watch(cli: &Cli, args: &WatchArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let since = match args.since.as_deref() {
        Some(raw) => {
            let duration = parse_since(raw).ok_or_else(|| {
                app_error(
                    2,
                    format!("Invalid --since value '{raw}'. Use forms like 30s, 10m, 2h, 1d."),
                )
            })?;
            Some(Utc::now() - chrono::Duration::from_std(duration).unwrap_or_default())
        }
        None => None,
    };

    let session = match args.session.as_deref() {
        Some("current") | Some("active") => {
            let active = active_state_file();
            if !active.exists() {
                return Err(app_error(2, "No active session for --session current."));
            }
            let state: SessionState = read_json(&active)?;
            Some(state.session_id)
        }
        Some(other) => Some(other.to_string()),
        None => None,
    };

    let filters = Filters {
        types: args.event_type.clone(),
        commands: args.command_filter.clone(),
        session,
        grep: args.grep.clone(),
        since,
    };

    let bus = event_bus_file();
    let color = use_color(cli);
    let mut out = io::stdout();

    let wants_backfill = args.all || args.since.is_some() || args.tail.is_some() || args.once;
    if wants_backfill {
        let mut lines: Vec<String> = Vec::new();
        // The rotated generation comes first so the merged view stays ordered.
        for path in [event_bus_rotated_file(), bus.clone()] {
            if let Ok(text) = fs::read_to_string(&path) {
                lines.extend(text.lines().map(|l| l.to_string()));
            }
        }
        let mut matched: Vec<(Value, String)> = lines
            .into_iter()
            .filter_map(|line| {
                let value: Value = serde_json::from_str(&line).ok()?;
                filters.matches(&value, &line).then_some((value, line))
            })
            .collect();
        if let Some(limit) = args.tail {
            let skip = matched.len().saturating_sub(limit);
            matched = matched.split_off(skip);
        }
        for (value, line) in matched {
            render(&mut out, cli, &value, &line, color);
        }
    }

    if args.once {
        let _ = out.flush();
        return Ok(0);
    }

    if !cli.quiet && !cli.json {
        print_out(cli, format!("watching {} — Ctrl-C to stop", bus.display()));
    }

    // Start at the end of what exists now; backfill above already covered history.
    let mut offset = fs::metadata(&bus).map(|m| m.len()).unwrap_or(0);
    let poll = Duration::from_millis(args.poll_ms.max(50));

    loop {
        let len = fs::metadata(&bus).map(|m| m.len()).unwrap_or(0);
        if len < offset {
            // Rotation or truncation: restart from the head of the new file.
            offset = 0;
        }
        if len > offset {
            match read_from(&bus, offset) {
                Ok((chunk, consumed)) => {
                    offset += consumed;
                    for line in chunk.lines() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        // A torn or partial record is skipped, not fatal.
                        let Ok(value) = serde_json::from_str::<Value>(line) else {
                            continue;
                        };
                        if filters.matches(&value, line) {
                            render(&mut out, cli, &value, line, color);
                        }
                    }
                }
                Err(_) => offset = len,
            }
        }
        thread::sleep(poll);
    }
}

/// Read appended bytes, returning only whole lines and how many bytes those
/// lines consumed so a partially-written record is re-read next poll.
fn read_from(path: &Path, offset: u64) -> Result<(String, u64), AppError> {
    let mut file = File::open(path)
        .map_err(|e| app_error(1, format!("Failed to open {}: {e}", path.display())))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| app_error(1, format!("Failed to seek {}: {e}", path.display())))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|e| app_error(1, format!("Failed to read {}: {e}", path.display())))?;

    let last_newline = buf.iter().rposition(|b| *b == b'\n');
    let Some(end) = last_newline else {
        return Ok((String::new(), 0));
    };
    let complete = &buf[..=end];
    Ok((
        String::from_utf8_lossy(complete).to_string(),
        complete.len() as u64,
    ))
}

fn use_color(cli: &Cli) -> bool {
    if cli.json || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    io::stdout().is_terminal()
}

fn render(out: &mut impl Write, cli: &Cli, event: &Value, raw: &str, color: bool) {
    if cli.json {
        let _ = writeln!(out, "{raw}");
        let _ = out.flush();
        return;
    }
    if cli.quiet {
        return;
    }

    let ts = event
        .get("ts")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Local).format("%H:%M:%S%.3f").to_string())
        .unwrap_or_else(|| "--:--:--.---".to_string());
    let command = event.get("command").and_then(|v| v.as_str()).unwrap_or("-");
    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("-");
    let session = event
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let level = event
        .get("level")
        .and_then(|v| v.as_str())
        .unwrap_or("info");
    let detail = describe(event);

    if color {
        let type_color = match level {
            "error" => "\x1b[31m",
            "warn" => "\x1b[33m",
            _ => "\x1b[32m",
        };
        let _ = writeln!(
            out,
            "\x1b[2m{ts}\x1b[0m  \x1b[36m{command:<16}\x1b[0m {type_color}{event_type:<28}\x1b[0m \x1b[2m{session:<15}\x1b[0m {detail}"
        );
    } else {
        let _ = writeln!(
            out,
            "{ts}  {command:<16} {event_type:<28} {session:<15} {detail}"
        );
    }
    let _ = out.flush();
}

/// Compact one-line summary of the domain payload (everything outside the
/// envelope), rendered as `key=value` pairs.
fn describe(event: &Value) -> String {
    let Some(map) = event.as_object() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in map {
        if RESERVED_KEYS.contains(&key.as_str()) || value.is_null() {
            continue;
        }
        let rendered = match value {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Array(items) => {
                if items.is_empty() {
                    continue;
                }
                format!("[{}]", items.len())
            }
            Value::Object(inner) => {
                if inner.is_empty() {
                    continue;
                }
                summarize_object(inner)
            }
            Value::Null => continue,
        };
        if rendered.is_empty() {
            continue;
        }
        parts.push(format!("{key}={rendered}"));
    }
    parts.join(" ")
}

fn summarize_object(map: &serde_json::Map<String, Value>) -> String {
    let inner: Vec<String> = map
        .iter()
        .filter(|(_, v)| match v {
            Value::Null => false,
            Value::Array(items) => !items.is_empty(),
            Value::String(s) => !s.is_empty(),
            _ => true,
        })
        .take(4)
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k}={s}"),
            Value::Object(_) => format!("{k}={{…}}"),
            Value::Array(items) => format!("{k}=[{}]", items.len()),
            other => format!("{k}={other}"),
        })
        .collect();
    if inner.is_empty() {
        return String::new();
    }
    format!("{{{}}}", inner.join(" "))
}

#[cfg(test)]
mod tests {
    use super::{describe, parse_since};
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn since_accepts_suffixes_and_bare_seconds() {
        assert_eq!(parse_since("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_since("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_since("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_since("1d"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_since("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_since(" 90 "), Some(Duration::from_secs(90)));
    }

    #[test]
    fn since_rejects_garbage() {
        assert_eq!(parse_since(""), None);
        assert_eq!(parse_since("soon"), None);
        assert_eq!(parse_since("-5m"), None);
    }

    #[test]
    fn describe_skips_envelope_and_nulls() {
        let event = json!({
            "v": 1,
            "ts": "2026-07-28T18:03:11.482Z",
            "type": "screenshot_taken",
            "command": "shot",
            "session_id": "20260728-180302",
            "seq": 2,
            "inv": "1-2",
            "pid": 3,
            "level": "info",
            "shot_id": 1,
            "audio_sec": 2.41,
            "app_name": null,
        });
        let described = describe(&event);
        assert!(described.contains("shot_id=1"));
        assert!(described.contains("audio_sec=2.41"));
        assert!(!described.contains("app_name"));
        assert!(!described.contains("session_id"));
        assert!(!described.contains("pid"));
    }
}
