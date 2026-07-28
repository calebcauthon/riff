//! Global event bus.
//!
//! Every `riff` invocation appends to `$RIFF_ROOT/events.jsonl` so that
//! `riff watch` can observe the whole tool, including commands that never
//! touch a session. Per-session `events.jsonl` files are untouched by this
//! module's writes — `append_session_event` mirrors them here instead.
//!
//! Records are flat: envelope keys sit alongside the domain payload so the
//! same `json!` value can be written to both files without reshaping.

use crate::error::{app_error, AppError};
use crate::paths::{event_bus_file, event_bus_rotated_file};
use crate::{bool_env_enabled, now_iso};
use serde_json::{json, Map, Value};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Envelope schema version. Bump when a field changes meaning.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// Envelope keys. Domain payloads must not use these names.
pub(crate) const RESERVED_KEYS: &[&str] = &[
    "v",
    "ts",
    "seq",
    "inv",
    "pid",
    "command",
    "type",
    "session_id",
    "level",
];

/// Long strings are clipped so one record stays inside a single append write
/// (keeping concurrent riff processes from interleaving mid-line) and so the
/// shared bus does not accumulate full clipboard and transcript text.
const MAX_STRING_LEN: usize = 200;

const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) const LEVEL_INFO: &str = "info";
pub(crate) const LEVEL_WARN: &str = "warn";
pub(crate) const LEVEL_ERROR: &str = "error";

struct BusCtx {
    inv: String,
    command: &'static str,
    pid: u32,
    seq: AtomicU64,
    started: Instant,
    enabled: bool,
}

static CTX: OnceLock<BusCtx> = OnceLock::new();

/// Bind this process to a command name. Called once from `run()`.
pub(crate) fn init(command: &'static str) {
    let pid = std::process::id();
    let epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let _ = CTX.set(BusCtx {
        inv: format!("{epoch_ms}-{pid}"),
        command,
        pid,
        seq: AtomicU64::new(0),
        started: Instant::now(),
        enabled: bool_env_enabled("RIFF_EVENT_BUS", true),
    });
}

fn max_bytes() -> u64 {
    env::var("RIFF_EVENT_BUS_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_BYTES)
}

/// Emit an event at `info`.
pub(crate) fn emit(event_type: &str, session_id: Option<&str>, payload: Value) {
    emit_leveled(LEVEL_INFO, event_type, session_id, payload);
}

/// Emit an event at an explicit level.
pub(crate) fn emit_leveled(
    level: &str,
    event_type: &str,
    session_id: Option<&str>,
    payload: Value,
) {
    let Some(ctx) = CTX.get() else {
        return;
    };
    if !ctx.enabled {
        return;
    }

    let mut map = match payload {
        Value::Object(m) => m,
        Value::Null => Map::new(),
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };

    let mut clipped = false;
    truncate_map(&mut map, &mut clipped);
    if clipped {
        map.insert("truncated".to_string(), json!(true));
    }

    // Envelope last so it always wins over a stray payload key.
    map.insert("v".to_string(), json!(SCHEMA_VERSION));
    map.entry("ts".to_string())
        .or_insert_with(|| json!(now_iso()));
    map.insert("type".to_string(), json!(event_type));
    map.insert(
        "seq".to_string(),
        json!(ctx.seq.fetch_add(1, Ordering::Relaxed)),
    );
    map.insert("inv".to_string(), json!(ctx.inv));
    map.insert("pid".to_string(), json!(ctx.pid));
    map.insert("command".to_string(), json!(ctx.command));
    map.insert("level".to_string(), json!(level));
    if let Some(session_id) = session_id {
        map.insert("session_id".to_string(), json!(session_id));
    }

    // A failed event write must never fail the command that produced it.
    let _ = append_bus_line(&Value::Object(map));
}

/// Mirror a per-session event onto the bus. The session id comes from the
/// session directory holding `events.jsonl`.
pub(crate) fn mirror_session_event(events_path: &Path, payload: &Value) {
    let session_id = events_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    let event_type = payload
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("session_event")
        .to_string();
    let level = payload
        .get("status")
        .and_then(|v| v.as_str())
        .map(|status| match status {
            "error" => LEVEL_ERROR,
            "skipped" => LEVEL_WARN,
            _ => LEVEL_INFO,
        })
        .unwrap_or(LEVEL_INFO);
    emit_leveled(level, &event_type, session_id, payload.clone());
}

fn truncate_map(map: &mut Map<String, Value>, clipped: &mut bool) {
    for (_, value) in map.iter_mut() {
        truncate_value(value, clipped);
    }
}

fn truncate_value(value: &mut Value, clipped: &mut bool) {
    match value {
        Value::String(s) => {
            if s.chars().count() > MAX_STRING_LEN {
                let head: String = s.chars().take(MAX_STRING_LEN).collect();
                *s = format!("{head}…");
                *clipped = true;
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                truncate_value(item, clipped);
            }
        }
        Value::Object(inner) => truncate_map(inner, clipped),
        _ => {}
    }
}

fn append_bus_line(payload: &Value) -> Result<(), AppError> {
    let path = event_bus_file();
    rotate_if_oversized(&path);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| app_error(1, format!("Failed to create {}: {e}", parent.display())))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| app_error(1, format!("Failed to open {}: {e}", path.display())))?;
    let mut line = serde_json::to_string(payload)
        .map_err(|e| app_error(1, format!("Failed to serialize event: {e}")))?;
    line.push('\n');
    file.write_all(line.as_bytes())
        .map_err(|e| app_error(1, format!("Failed to append {}: {e}", path.display())))
}

fn rotate_if_oversized(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() <= max_bytes() {
        return;
    }
    let _ = fs::rename(path, event_bus_rotated_file());
}

// ---------------------------------------------------------------------------
// Command lifecycle
// ---------------------------------------------------------------------------

pub(crate) fn command_started(args: Value) {
    emit("command_started", None, json!({ "args": args }));
}

pub(crate) fn command_finished(result: &Result<i32, AppError>) {
    let duration_ms = CTX
        .get()
        .map(|ctx| crate::round3(ctx.started.elapsed().as_secs_f64() * 1000.0))
        .unwrap_or(0.0);
    match result {
        Ok(0) => emit(
            "command_finished",
            None,
            json!({ "exit_code": 0, "duration_ms": duration_ms }),
        ),
        Ok(code) => emit_leveled(
            LEVEL_WARN,
            "command_finished",
            None,
            json!({ "exit_code": code, "duration_ms": duration_ms }),
        ),
        Err(err) => emit_leveled(
            LEVEL_ERROR,
            "command_failed",
            None,
            json!({
                "exit_code": err.code,
                "duration_ms": duration_ms,
                "error": err.message,
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{truncate_value, MAX_STRING_LEN, RESERVED_KEYS, SCHEMA_VERSION};
    use serde_json::json;

    #[test]
    fn long_strings_are_clipped_and_flagged() {
        let mut value = json!({ "text": "x".repeat(MAX_STRING_LEN + 50) });
        let mut clipped = false;
        truncate_value(&mut value, &mut clipped);
        assert!(clipped);
        let text = value["text"].as_str().expect("text");
        assert_eq!(text.chars().count(), MAX_STRING_LEN + 1); // clipped + ellipsis
    }

    #[test]
    fn short_strings_are_left_alone() {
        let mut value = json!({ "nested": { "text": "hello" }, "list": ["a", "b"] });
        let mut clipped = false;
        truncate_value(&mut value, &mut clipped);
        assert!(!clipped);
        assert_eq!(value["nested"]["text"], json!("hello"));
    }

    #[test]
    fn truncation_recurses_into_arrays_and_objects() {
        let long = "y".repeat(MAX_STRING_LEN + 10);
        let mut value = json!({ "list": [{ "deep": long }] });
        let mut clipped = false;
        truncate_value(&mut value, &mut clipped);
        assert!(clipped);
    }

    #[test]
    fn envelope_contract_is_stable() {
        assert_eq!(SCHEMA_VERSION, 1);
        assert!(RESERVED_KEYS.contains(&"session_id"));
        assert!(RESERVED_KEYS.contains(&"inv"));
    }
}
