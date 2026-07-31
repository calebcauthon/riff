//! Shared read side of the global event bus.
//!
//! `riff watch` and the daemon's `/subscribe` fan-out both consume the bus by
//! tailing `$RIFF_ROOT/events.jsonl`. This module owns that mechanism so the
//! two stay in lockstep: backfill over the retained generations, then
//! whole-line incremental reads that survive rotation.

use crate::error::{app_error, AppError};
use crate::paths::{event_bus_file, event_bus_rotated_file};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Event filters shared by `riff watch` and the daemon subscribe API.
/// All populated criteria must match (AND); values within one criterion OR.
#[derive(Default)]
pub(crate) struct EventFilters {
    pub(crate) types: Vec<String>,
    pub(crate) commands: Vec<String>,
    pub(crate) session: Option<String>,
    pub(crate) grep: Option<String>,
    pub(crate) since: Option<DateTime<Utc>>,
}

impl EventFilters {
    pub(crate) fn matches(&self, event: &Value, raw: &str) -> bool {
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

/// Every retained bus line, rotated generation first so the merged view stays
/// ordered.
pub(crate) fn read_retained_lines() -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for path in [event_bus_rotated_file(), event_bus_file()] {
        if let Ok(text) = fs::read_to_string(&path) {
            lines.extend(text.lines().map(|l| l.to_string()));
        }
    }
    lines
}

/// Incremental whole-line reader over the live bus file.
///
/// Tracks a byte offset; `poll()` returns any newly completed lines. A file
/// that shrank (rotation or truncation) restarts from the head of the new
/// generation.
pub(crate) struct BusTailer {
    path: PathBuf,
    offset: u64,
}

impl BusTailer {
    /// Start at the end of what exists now — callers backfill separately.
    pub(crate) fn from_end() -> Self {
        let path = event_bus_file();
        let offset = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self { path, offset }
    }

    pub(crate) fn poll(&mut self) -> Vec<String> {
        let len = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Rotation or truncation: restart from the head of the new file.
            self.offset = 0;
        }
        if len <= self.offset {
            return Vec::new();
        }
        match read_from(&self.path, self.offset) {
            Ok((chunk, consumed)) => {
                self.offset += consumed;
                chunk
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| line.to_string())
                    .collect()
            }
            Err(_) => {
                self.offset = len;
                Vec::new()
            }
        }
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

#[cfg(test)]
mod tests {
    use super::EventFilters;
    use serde_json::json;

    #[test]
    fn empty_filters_match_everything() {
        let filters = EventFilters::default();
        let event = json!({ "type": "anything" });
        assert!(filters.matches(&event, "{\"type\":\"anything\"}"));
    }

    #[test]
    fn type_and_command_filters_are_anded() {
        let filters = EventFilters {
            types: vec!["a".to_string()],
            commands: vec!["stop".to_string()],
            ..Default::default()
        };
        assert!(filters.matches(&json!({ "type": "a", "command": "stop" }), ""));
        assert!(!filters.matches(&json!({ "type": "a", "command": "start" }), ""));
        assert!(!filters.matches(&json!({ "type": "b", "command": "stop" }), ""));
    }
}
