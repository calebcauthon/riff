//! ElevenLabs engine: streaming transcription via Scribe v2 Realtime.
//!
//! The inversion that makes this fast: transcription happens *during* the
//! recording, not after it. A sidecar tails the growing WAV, streams 1-second
//! PCM frames over a WebSocket, and writes committed text into the shared
//! transcript contract as it arrives. By the time `riff stop` runs, the only
//! outstanding work is a final commit — roughly flat regardless of how long you
//! talked, instead of Parakeet's inference cost that scales with the tail.
//!
//! Failures are surfaced, never papered over. If the stream dies, the key is
//! missing, or quota is exhausted, stop reports `status: "error"` with whatever
//! text was committed before the failure. It does not silently fall back to a
//! local transcribe, because a silent fallback would hand you a slow session
//! while you believed you were on the fast path.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cli::Cli;
use crate::engine::{load_chunked_transcript, StopCtx, TranscriptionEngine};
use crate::error::{app_error, AppError};
use crate::history::read_jsonl_values;
use crate::models::SessionState;
use crate::transcription::{resolve_python_bin, resource_dir};
use crate::{
    append_session_event, now_iso, print_verbose, process_is_alive, round3, send_signal,
};

/// `SIGUSR1` asks the sidecar to commit the current segment (chunk / pause).
const SIG_COMMIT: i32 = libc::SIGUSR1;
/// `SIGUSR2` asks the sidecar to make a final commit, flush, and exit.
const SIG_FINALIZE: i32 = libc::SIGUSR2;

pub(crate) struct ElevenLabsEngine;

impl TranscriptionEngine for ElevenLabsEngine {
    fn id(&self) -> &'static str {
        "elevenlabs"
    }

    /// Check credentials and the sidecar script before anything is spawned.
    ///
    /// Recording for ten minutes and only then learning the key was missing is
    /// the worst possible time to find out, and the whole point of this engine
    /// is not waiting.
    fn preflight(&self, _cli: &Cli) -> Result<(), AppError> {
        require_api_key()?;
        require_stream_script()?;
        require_stream_python()?;
        Ok(())
    }

    fn on_start(&self, state: &SessionState, cli: &Cli) -> Result<Option<i32>, AppError> {
        let api_key = require_api_key()?;
        let script_path = require_stream_script()?;
        let python_bin = require_stream_python()?;
        let session_dir = PathBuf::from(&state.session_dir);
        let log_path = session_dir.join("elevenlabs-stream.log");
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| app_error(1, format!("Failed to open {}: {e}", log_path.display())))?;
        let log_err = log_file
            .try_clone()
            .map_err(|e| app_error(1, format!("Failed to clone stream log handle: {e}")))?;

        let mut cmd = Command::new(&python_bin);
        cmd.arg(&script_path)
            .arg("--audio")
            .arg(&state.audio_path)
            .arg("--session-dir")
            .arg(&session_dir)
            .arg("--events-path")
            .arg(&state.events_path)
            .arg("--model-id")
            .arg(model_id())
            .arg("--sample-rate")
            .arg("16000")
            .arg("--commit-strategy")
            .arg(commit_strategy());

        if let Some(lang) = std::env::var("RIFF_ELEVENLABS_LANGUAGE")
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            cmd.arg("--language-code").arg(lang);
        }
        if let Some(base) = std::env::var("RIFF_ELEVENLABS_WS_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            cmd.arg("--ws-url").arg(base);
        }

        // The key goes through the environment, never argv, so it stays out of
        // `ps` output and the ffmpeg/session logs.
        cmd.env("ELEVENLABS_API_KEY", api_key)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err));

        print_verbose(
            cli,
            format!(
                "Starting ElevenLabs stream sidecar: {} {}",
                python_bin,
                script_path.display()
            ),
        );

        let child = cmd.spawn().map_err(|e| {
            app_error(
                6,
                format!("Failed to start ElevenLabs stream sidecar ({python_bin}): {e}"),
            )
        })?;
        let pid = child.id() as i32;

        append_session_event(
            &PathBuf::from(&state.events_path),
            &json!({
                "ts": now_iso(),
                "type": "elevenlabs_stream_started",
                "pid": pid,
                "model_id": model_id(),
                "script": script_path.display().to_string(),
                "log_path": log_path.display().to_string(),
            }),
        )?;

        Ok(Some(pid))
    }

    fn on_chunk(
        &self,
        state: &mut SessionState,
        cli: &Cli,
        reason: &str,
        forced_end_sec: Option<f64>,
    ) -> Result<Value, AppError> {
        let events_path = PathBuf::from(&state.events_path);
        let end_sec = forced_end_sec.unwrap_or_else(|| crate::engine::audio_elapsed_sec(state));
        let start_sec = state.transcription_cursor_sec.max(0.0);

        let Some(pid) = state.transcription_watcher_pid.filter(|p| process_is_alive(*p)) else {
            return Ok(json!({
                "status": "error",
                "reason": "stream_sidecar_not_running",
                "requested_reason": reason,
            }));
        };

        let before = count_chunk_events(&events_path);
        send_signal(pid, SIG_COMMIT)
            .map_err(|e| app_error(1, format!("Failed to signal ElevenLabs sidecar: {e}")))?;
        print_verbose(cli, format!("Requested ElevenLabs commit ({reason})."));

        // A commit round-trip is ~150ms; give it room but never block for long.
        let observed = wait_for_new_chunk_event(&events_path, before, Duration::from_millis(3000));
        state.transcription_cursor_sec = end_sec.max(state.transcription_cursor_sec);

        Ok(json!({
            "status": if observed { "ok" } else { "pending" },
            "reason": reason,
            "mode": "stream",
            "start_sec": round3(start_sec),
            "end_sec": round3(end_sec),
            "committed": observed,
        }))
    }

    fn on_stop(&self, state: &mut SessionState, cli: &Cli, _ctx: &StopCtx) -> (String, Value) {
        let session_dir = PathBuf::from(&state.session_dir);
        let events_path = PathBuf::from(&state.events_path);
        let t_finalize = Instant::now();

        let mut finalize_status = "ok";
        let mut finalize_detail = Value::Null;

        if let Some(pid) = state.transcription_watcher_pid {
            if process_is_alive(pid) {
                if let Err(e) = send_signal(pid, SIG_FINALIZE) {
                    finalize_status = "error";
                    finalize_detail = json!(format!("Failed to signal sidecar: {e}"));
                } else {
                    print_verbose(cli, format!("Sent final commit to sidecar pid={pid}."));
                    // Wait for the transcript to be final, not for the process
                    // to exit. Python interpreter teardown is ~150ms of pure
                    // latency that buys nothing once the text is on disk.
                    if wait_for_event(&events_path, "elevenlabs_stream_finished", finalize_timeout())
                    {
                        // Reap in the background so shutdown stays off the
                        // critical path.
                        let _ = send_signal(pid, libc::SIGTERM);
                    } else {
                        finalize_status = "error";
                        finalize_detail = json!("Sidecar did not finish final commit in time.");
                        crate::stop_transcription_watcher(pid, cli);
                    }
                }
            } else {
                // Died mid-session. Its error event (if any) is picked up below.
                finalize_status = "error";
                finalize_detail = json!("Stream sidecar exited before stop.");
            }
        } else {
            finalize_status = "error";
            finalize_detail = json!("No stream sidecar was running for this session.");
        }

        let finalize_ms = round3(t_finalize.elapsed().as_secs_f64() * 1000.0);
        state.transcription_paused = false;
        state.transcription_pause_started_sec = None;

        let (transcript, mut meta) =
            load_chunked_transcript(&session_dir, &events_path, "elevenlabs_stream");

        // Any error the sidecar reported wins over an otherwise-ok read: a
        // partial transcript that looks complete is worse than a loud failure.
        let stream_errors = collect_stream_errors(&events_path);
        if let Some(obj) = meta.as_object_mut() {
            obj.insert("engine".to_string(), json!("elevenlabs"));
            obj.insert("model_id".to_string(), json!(model_id()));
            obj.insert("finalize_ms".to_string(), json!(finalize_ms));
            obj.insert("finalize_status".to_string(), json!(finalize_status));
            if !finalize_detail.is_null() {
                obj.insert("finalize_detail".to_string(), finalize_detail);
            }
            if !stream_errors.is_empty() {
                obj.insert("stream_errors".to_string(), json!(stream_errors));
            }
            if finalize_status != "ok" || !stream_errors.is_empty() {
                obj.insert("status".to_string(), json!("error"));
                obj.insert("partial".to_string(), json!(!transcript.is_empty()));
            }
        }

        (transcript, meta)
    }
}

fn model_id() -> String {
    std::env::var("RIFF_ELEVENLABS_MODEL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "scribe_v2_realtime".to_string())
}

fn require_api_key() -> Result<String, AppError> {
    resolve_api_key().ok_or_else(|| {
        app_error(
            2,
            "ElevenLabs engine requires an API key. Set RIFF_ELEVENLABS_API_KEY \
             (or ELEVENLABS_API_KEY).",
        )
    })
}

/// Find an interpreter that can actually import `websockets`.
///
/// The trap this exists to catch: `pip install websockets` installs into
/// whichever python owns the `pip` on your PATH, which is routinely *not* the
/// interpreter riff runs sidecars with (a bundled runtime, or a pinned 3.10-3.12).
/// Without this check the sidecar dies on import and you only find out at stop,
/// with an empty transcript and no obvious cause.
fn require_stream_python() -> Result<String, AppError> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(explicit) = std::env::var("RIFF_ELEVENLABS_PYTHON") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            candidates.push(trimmed.to_string());
        }
    }
    candidates.push(resolve_python_bin(None));
    for fallback in ["python3.12", "python3.11", "python3.13", "python3"] {
        candidates.push(fallback.to_string());
    }

    let mut tried: Vec<String> = Vec::new();
    for candidate in candidates {
        if tried.contains(&candidate) {
            continue;
        }
        match Command::new(&candidate)
            .args(["-c", "import websockets"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => return Ok(candidate),
            // A missing interpreter and a missing package are different
            // problems; only the latter is worth naming in the error.
            Ok(_) => tried.push(candidate),
            Err(_) => continue,
        }
    }

    let hint = tried
        .first()
        .cloned()
        .unwrap_or_else(|| resolve_python_bin(None));
    Err(app_error(
        2,
        format!(
            "The ElevenLabs engine needs the 'websockets' package, but none of the \
             interpreters riff can use has it installed{}.\n\nInstall it into the \
             interpreter riff actually runs:\n    \"{hint}\" -m pip install -r \
             scripts/elevenlabs-requirements.txt\n\nNote that a bare `pip install` \
             targets whichever python owns your PATH's pip, which is often a \
             different interpreter. Set RIFF_ELEVENLABS_PYTHON to choose one \
             explicitly.",
            if tried.is_empty() {
                String::new()
            } else {
                format!(" (tried: {})", tried.join(", "))
            }
        ),
    ))
}

fn require_stream_script() -> Result<PathBuf, AppError> {
    resolve_stream_script().ok_or_else(|| {
        app_error(
            2,
            "Could not locate elevenlabs_stream.py. Set RIFF_ELEVENLABS_SCRIPT to its path.",
        )
    })
}

/// `vad` lets the server finalize segments during natural pauses, so most of
/// the transcript is already written when stop runs. `manual` defers everything
/// to an explicit commit, which pushes the work back into stop.
fn commit_strategy() -> String {
    match std::env::var("RIFF_ELEVENLABS_COMMIT_STRATEGY")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("manual") => "manual".to_string(),
        _ => "vad".to_string(),
    }
}

fn resolve_api_key() -> Option<String> {
    for key in ["RIFF_ELEVENLABS_API_KEY", "ELEVENLABS_API_KEY"] {
        if let Ok(v) = std::env::var(key) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn finalize_timeout() -> Duration {
    let secs = std::env::var("RIFF_ELEVENLABS_FINALIZE_TIMEOUT_SEC")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(8.0)
        .clamp(1.0, 60.0);
    Duration::from_secs_f64(secs)
}

fn resolve_stream_script() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RIFF_ELEVENLABS_SCRIPT") {
        let path = PathBuf::from(explicit.trim());
        if path.exists() {
            return Some(path);
        }
    }
    if let Some(dir) = resource_dir() {
        let candidate = dir.join("scripts").join("elevenlabs_stream.py");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Poll the session log until an event of `event_type` appears.
///
/// Polls fast: this sits directly on `riff stop`'s critical path, so a coarse
/// interval would show up as latency the user actually feels.
fn wait_for_event(events_path: &Path, event_type: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let seen = read_jsonl_values(events_path)
            .iter()
            .any(|e| e.get("type").and_then(|v| v.as_str()) == Some(event_type));
        if seen {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn count_chunk_events(events_path: &Path) -> usize {
    read_jsonl_values(events_path)
        .iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("transcript_chunk"))
        .count()
}

fn wait_for_new_chunk_event(events_path: &Path, before: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if count_chunk_events(events_path) > before {
            return true;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    false
}

fn collect_stream_errors(events_path: &Path) -> Vec<Value> {
    read_jsonl_values(events_path)
        .into_iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("elevenlabs_stream_error"))
        .collect()
}
