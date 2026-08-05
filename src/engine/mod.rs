//! Transcription engines.
//!
//! An engine owns everything that differs between transcription backends across
//! the session lifecycle: what the recorder must emit, what runs alongside the
//! recording, and how the final transcript is produced at stop.
//!
//! Engines converge on one provider-neutral contract:
//!
//!   * `session_dir/transcript.txt` — the merged transcript text
//!   * `transcript_chunk` events in the session JSONL
//!
//! Everything downstream of that (note rendering, output hooks, reporting,
//! history, `riff watch`) reads only the contract and never learns which engine
//! produced it. That is what keeps backend selection to a single dispatch point
//! instead of branches scattered through the command implementations.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::cli::Cli;
use crate::error::AppError;
use crate::models::SessionState;

pub(crate) mod elevenlabs;
pub(crate) mod parakeet;

/// Everything `on_stop` needs beyond the session state itself.
pub(crate) struct StopCtx<'a> {
    /// Resolved stop arguments (python bin, parakeet script/model overrides).
    pub stop_args: &'a crate::cli::StopArgs,
}

/// The lifecycle contract every transcription backend implements.
///
/// Note there is deliberately no recorder hook. The recorder already writes
/// 16 kHz mono `pcm_s16le`, which is exactly the format ElevenLabs wants, so a
/// streaming engine can tail the growing WAV rather than needing its own sink.
/// A tee via FIFO would stall `riff start` until a reader attached, which is
/// not a trade worth making for a command that currently returns in ~140ms.
pub(crate) trait TranscriptionEngine {
    fn id(&self) -> &'static str;

    /// Validate configuration before `riff start` causes any side effects.
    ///
    /// Runs before the session directory is created and before the recorder is
    /// spawned, so a misconfigured engine fails clean instead of leaving an
    /// orphaned ffmpeg behind.
    fn preflight(&self, _cli: &Cli) -> Result<(), AppError> {
        Ok(())
    }

    /// Called during `riff start` once the recorder is live.
    ///
    /// Returns the pid of any sidecar process that should be tracked in
    /// `SessionState::transcription_watcher_pid`. Returning `Err` aborts the
    /// start, so misconfiguration surfaces before you record into a void.
    fn on_start(&self, state: &SessionState, cli: &Cli) -> Result<Option<i32>, AppError>;

    /// Called for `riff chunk`, `riff pause`, and the final flush at stop.
    ///
    /// Must append a `transcript_chunk` event and advance
    /// `SessionState::transcription_cursor_sec`.
    fn on_chunk(
        &self,
        state: &mut SessionState,
        cli: &Cli,
        reason: &str,
        forced_end_sec: Option<f64>,
    ) -> Result<Value, AppError>;

    /// Called during `riff stop` once the recorder has been stopped.
    ///
    /// Returns the final transcript text and its metadata. This is where the
    /// engines diverge most: Parakeet still has inference work to do here,
    /// while a streaming engine only has to flush and collect.
    fn on_stop(&self, state: &mut SessionState, cli: &Cli, ctx: &StopCtx) -> (String, Value);
}

/// Resolve the engine for a session.
///
/// This is the single dispatch point. `riff stop` never sees the `--engine`
/// flag — the id is recorded in `SessionState` at start, so every later command
/// simply reads it back.
pub(crate) fn engine_for(state: &SessionState) -> Box<dyn TranscriptionEngine> {
    engine_by_id(&state.engine)
}

pub(crate) fn engine_by_id(id: &str) -> Box<dyn TranscriptionEngine> {
    match normalize_engine_id(id) {
        "elevenlabs" => Box::new(elevenlabs::ElevenLabsEngine),
        _ => Box::new(parakeet::ParakeetEngine),
    }
}

/// Sessions recorded before engines existed have an empty id; treat as parakeet.
pub(crate) fn normalize_engine_id(id: &str) -> &str {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        "parakeet"
    } else {
        trimmed
    }
}

pub(crate) const KNOWN_ENGINES: [&str; 2] = ["parakeet", "elevenlabs"];

/// Resolve the requested engine from the CLI flag, then `RIFF_ENGINE`, then the
/// default. Returns an error for unknown names rather than silently falling
/// back, so a typo does not quietly cost you the fast path.
pub(crate) fn resolve_engine_id(flag: Option<&str>) -> Result<String, AppError> {
    let requested = flag
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            std::env::var("RIFF_ENGINE")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| "parakeet".to_string());

    let lowered = requested.to_ascii_lowercase();
    if KNOWN_ENGINES.contains(&lowered.as_str()) {
        Ok(lowered)
    } else {
        Err(crate::error::app_error(
            2,
            format!(
                "Unknown transcription engine {:?}. Known engines: {}.",
                requested,
                KNOWN_ENGINES.join(", ")
            ),
        ))
    }
}

// ---------------------------------------------------------------------------
// Shared transcript contract helpers
//
// Both engines produce the same artifacts, so the readers and mergers below are
// engine-neutral and live here rather than in either backend.
// ---------------------------------------------------------------------------

pub(crate) fn audio_elapsed_sec(state: &SessionState) -> f64 {
    crate::get_audio_duration_sec(Path::new(&state.audio_path))
        .unwrap_or_else(|| (crate::unix_now() - state.started_at_epoch).max(0.0))
}

pub(crate) fn next_transcript_chunk_id(events: &[Value]) -> usize {
    let mut max_id = 0usize;
    for e in events {
        if e.get("type").and_then(|v| v.as_str()) == Some("transcript_chunk") {
            if let Some(id) = e.get("id").and_then(|v| v.as_u64()) {
                max_id = max_id.max(id as usize);
            }
        }
    }
    max_id + 1
}

pub(crate) fn merge_manual_chunk_text(existing: &str, chunk: &str) -> String {
    let existing = existing.trim();
    let chunk = chunk.trim();
    if existing.is_empty() {
        return chunk.to_string();
    }
    if chunk.is_empty() {
        return existing.to_string();
    }
    format!("{existing}\n\n{chunk}")
}

/// Append `chunk` to `transcript.txt`, creating it if needed.
pub(crate) fn append_transcript_text(session_dir: &Path, chunk: &str) -> Result<(), AppError> {
    let transcript_path = session_dir.join("transcript.txt");
    let existing = fs::read_to_string(&transcript_path).unwrap_or_default();
    let merged = merge_manual_chunk_text(&existing, chunk);
    fs::write(&transcript_path, format!("{merged}\n")).map_err(|e| {
        crate::error::app_error(
            1,
            format!(
                "Failed to write merged transcript {}: {e}",
                transcript_path.display()
            ),
        )
    })
}

/// Read back the transcript assembled from `transcript_chunk` events.
///
/// Engine-neutral: it only knows the contract, not who wrote it. The `method`
/// reported in the metadata is supplied by the calling engine.
pub(crate) fn load_chunked_transcript(
    session_dir: &Path,
    events_path: &Path,
    method: &str,
) -> (String, Value) {
    let transcript_path = session_dir.join("transcript.txt");
    let transcript_raw = fs::read_to_string(&transcript_path)
        .unwrap_or_default()
        .trim()
        .to_string();

    let events = crate::history::read_jsonl_values(events_path);
    let mut chunk_count = 0usize;
    let mut chunk_seconds = 0.0f64;
    let mut chunk_mode = "manual";
    let mut stopping_seen = false;
    let mut stop_reason = String::new();
    let mut status_counts: HashMap<String, usize> = HashMap::new();

    for e in &events {
        let et = e.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        if et == "session_stopping" {
            stopping_seen = true;
        }
        if et != "transcript_chunk" {
            continue;
        }
        chunk_count = chunk_count.saturating_add(1);
        let start_sec = e.get("start_sec").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let end_sec = e
            .get("end_sec")
            .and_then(|v| v.as_f64())
            .unwrap_or(start_sec);
        chunk_seconds += (end_sec - start_sec).max(0.0);

        if let Some(mode) = e.get("mode").and_then(|v| v.as_str()) {
            chunk_mode = mode;
        }
        if let Some(reason) = e.get("reason").and_then(|v| v.as_str()) {
            stop_reason = reason.to_string();
        }
        if let Some(status) = e.get("status").and_then(|v| v.as_str()) {
            *status_counts.entry(status.to_string()).or_insert(0) += 1;
        }
    }

    let ok_chunks = status_counts.get("ok").copied().unwrap_or(0);
    let skipped_chunks = status_counts.get("skipped").copied().unwrap_or(0);
    let errored_chunks = status_counts.get("error").copied().unwrap_or(0);
    let status = if transcript_raw.is_empty() && chunk_count == 0 {
        "empty"
    } else if errored_chunks > 0 && ok_chunks == 0 {
        "error"
    } else {
        "ok"
    };

    (
        transcript_raw,
        json!({
            "status": status,
            "method": method,
            "mode": chunk_mode,
            "chunks": chunk_count,
            "chunks_ok": ok_chunks,
            "chunks_skipped": skipped_chunks,
            "chunks_error": errored_chunks,
            "chunk_audio_sec": crate::round3(chunk_seconds),
            "stopping_seen": stopping_seen,
            "stop_reason": if stop_reason.is_empty() { Value::Null } else { Value::String(stop_reason) }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_manual_chunk_text_uses_double_newline_separator() {
        let merged = merge_manual_chunk_text("first chunk", "second chunk");
        assert_eq!(merged, "first chunk\n\nsecond chunk");
    }

    #[test]
    fn merge_manual_chunk_text_trims_outer_whitespace() {
        let merged = merge_manual_chunk_text("  first  ", "  second  ");
        assert_eq!(merged, "first\n\nsecond");
    }

    #[test]
    fn empty_engine_id_is_parakeet() {
        assert_eq!(normalize_engine_id(""), "parakeet");
        assert_eq!(normalize_engine_id("   "), "parakeet");
        assert_eq!(normalize_engine_id("elevenlabs"), "elevenlabs");
    }

    #[test]
    fn resolve_engine_id_rejects_unknown_names() {
        assert!(resolve_engine_id(Some("nope")).is_err());
        assert_eq!(resolve_engine_id(Some("elevenlabs")).unwrap(), "elevenlabs");
        assert_eq!(resolve_engine_id(Some("ElevenLabs")).unwrap(), "elevenlabs");
    }
}
