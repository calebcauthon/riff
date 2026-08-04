//! Parakeet engine: local, file-based transcription.
//!
//! Work is deferred rather than streamed. A watcher slices the growing WAV and
//! transcribes completed segments during the session, but whatever audio remains
//! past the cursor is only transcribed at stop — which is why `on_stop` costs
//! inference time proportional to the unflushed tail.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cli::Cli;
use crate::engine::{
    append_transcript_text, audio_elapsed_sec, load_chunked_transcript, next_transcript_chunk_id,
    StopCtx, TranscriptionEngine,
};
use crate::error::{app_error, AppError};
use crate::history::read_jsonl_values;
use crate::models::SessionState;
use crate::transcription::{
    ensure_parakeet_server, parakeet_server_base_url, parakeet_server_enabled,
    resolve_parakeet_model, resolve_parakeet_script, resolve_python_bin, run_transcription,
    transcribe_via_parakeet_server,
};
use crate::{
    append_session_event, command_exists, now_iso, print_verbose, round3,
    spawn_transcription_watcher, stop_transcription_watcher, wait_for_transcription_watcher,
};

pub(crate) struct ParakeetEngine;

impl TranscriptionEngine for ParakeetEngine {
    fn id(&self) -> &'static str {
        "parakeet"
    }

    fn on_start(&self, state: &SessionState, cli: &Cli) -> Result<Option<i32>, AppError> {
        Ok(spawn_transcription_watcher(state, cli))
    }

    fn on_chunk(
        &self,
        state: &mut SessionState,
        cli: &Cli,
        reason: &str,
        forced_end_sec: Option<f64>,
    ) -> Result<Value, AppError> {
        process_manual_chunk(state, cli, reason, forced_end_sec)
    }

    fn on_stop(&self, state: &mut SessionState, cli: &Cli, ctx: &StopCtx) -> (String, Value) {
        let session_dir = PathBuf::from(&state.session_dir);
        let events_path = PathBuf::from(&state.events_path);

        // Drain the watcher first so its in-flight chunk lands before we flush.
        let mut forced_stop = false;
        let mut watcher_wait_ms = 0.0;
        if let Some(pid) = state.transcription_watcher_pid {
            print_verbose(
                cli,
                format!("Waiting up to 12s for transcription watcher pid={pid} to finish."),
            );
            let (finished, waited_ms) =
                wait_for_transcription_watcher(pid, Duration::from_secs(12), cli);
            watcher_wait_ms = waited_ms;
            if !finished {
                forced_stop = true;
                print_verbose(
                    cli,
                    format!("Transcription watcher pid={pid} did not finish in time; forcing stop."),
                );
                stop_transcription_watcher(pid, cli);
            }
        }

        // If any chunking happened, the transcript is assembled from chunk
        // events and we only owe the tail. Otherwise fall back to transcribing
        // the whole file. This branch is local Parakeet knowledge, not a
        // backend selection, so it belongs here rather than in `cmd_stop`.
        let use_chunked = state.transcription_watcher_pid.is_some()
            || state.transcription_cursor_sec > 0.05
            || state.transcription_paused;

        let t_flush_total = Instant::now();
        let stop_flush_meta = if use_chunked {
            print_verbose(
                cli,
                format!(
                    "Stop transcription strategy: chunked_flush (watcher_pid={:?} cursor_sec={:.3} paused={})",
                    state.transcription_watcher_pid,
                    state.transcription_cursor_sec,
                    state.transcription_paused
                ),
            );
            let t_flush = Instant::now();
            let meta = match process_manual_chunk(state, cli, "stop_flush", None) {
                Ok(meta) => meta,
                Err(e) => json!({"status": "error", "reason": e.message}),
            };
            print_verbose(
                cli,
                format!(
                    "Stop flush result ({}ms): {meta}",
                    round3(t_flush.elapsed().as_secs_f64() * 1000.0)
                ),
            );
            meta
        } else {
            print_verbose(cli, "Stop transcription strategy: full_transcribe_on_stop");
            json!({"status": "skipped", "reason": "full_transcribe_on_stop"})
        };

        let stop_flush_ms = round3(t_flush_total.elapsed().as_secs_f64() * 1000.0);
        state.transcription_paused = false;
        state.transcription_pause_started_sec = None;

        let (transcript, mut meta) = if use_chunked {
            load_chunked_transcript(&session_dir, &events_path, "manual_chunked")
        } else {
            run_transcription(state, &session_dir, ctx.stop_args, cli)
        };

        if let Some(obj) = meta.as_object_mut() {
            obj.insert("engine".to_string(), json!("parakeet"));
            obj.insert("forced_watcher_stop".to_string(), json!(forced_stop));
            obj.insert("watcher_wait_ms".to_string(), json!(round3(watcher_wait_ms)));
            obj.insert("stop_flush_ms".to_string(), json!(stop_flush_ms));
            obj.insert("stop_flush".to_string(), stop_flush_meta);
        }
        (transcript, meta)
    }
}

fn extract_audio_segment(
    source_audio: &Path,
    start_sec: f64,
    end_sec: f64,
    target_audio: &Path,
) -> Result<(), AppError> {
    if end_sec <= start_sec {
        return Err(app_error(
            1,
            "Invalid chunk boundary: end <= start for audio segment extract.",
        ));
    }
    let duration = (end_sec - start_sec).max(0.0);
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .arg("-ss")
        .arg(format!("{start_sec:.3}"))
        .arg("-t")
        .arg(format!("{duration:.3}"))
        .arg("-i")
        .arg(source_audio)
        .args(["-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"])
        .arg(target_audio)
        .output()
        .map_err(|e| app_error(1, format!("Failed to run ffmpeg for chunk extract: {e}")))?;
    if !output.status.success() || !target_audio.exists() {
        return Err(app_error(
            1,
            format!(
                "ffmpeg chunk extract failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(())
}

fn transcribe_chunk_audio(chunk_audio: &Path, chunk_out_txt: &Path, cli: &Cli) -> (String, Value) {
    let script = resolve_parakeet_script(None);
    let Some(script_path) = script else {
        return (
            String::new(),
            json!({
                "status": "skipped",
                "reason": "No transcription configured. Set RIFF_PARAKEET_SCRIPT or use --parakeet-script."
            }),
        );
    };

    let python_bin = resolve_python_bin(None);
    let model = resolve_parakeet_model(None);
    let cmd_for_log = format!(
        "{} {} --audio {} --out-txt {} --model {}",
        python_bin,
        script_path.display(),
        chunk_audio.display(),
        chunk_out_txt.display(),
        model
    );
    print_verbose(
        cli,
        format!("Running chunk transcription (one-shot): {cmd_for_log}"),
    );

    let mut server_error: Option<Value> = None;
    if parakeet_server_enabled() && command_exists("curl") {
        let base_url = parakeet_server_base_url();
        let warmup =
            ensure_parakeet_server(&python_bin, &script_path, &model, cli, true, None, "chunk");
        if let Some(identity) = warmup.identity.as_ref() {
            match transcribe_via_parakeet_server(
                &base_url,
                chunk_audio,
                chunk_out_txt,
                &model,
                identity,
            ) {
                Ok(result) => return result,
                Err(error) => server_error = Some(error),
            }
        }
    }

    let output = Command::new(&python_bin)
        .arg(&script_path)
        .arg("--audio")
        .arg(chunk_audio)
        .arg("--out-txt")
        .arg(chunk_out_txt)
        .arg("--model")
        .arg(&model)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let txt = if chunk_out_txt.exists() {
                fs::read_to_string(chunk_out_txt).unwrap_or_default()
            } else {
                String::from_utf8_lossy(&out.stdout).to_string()
            };
            let mut meta = json!({
                "status": "ok",
                "method": "parakeet_python",
                "model": model,
                "script": script_path.display().to_string(),
            });
            if let Some(err) = server_error {
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert("server_fallback".to_string(), err);
                }
            }
            (txt.trim().to_string(), meta)
        }
        Ok(out) => {
            let mut meta = json!({
                "status": "error",
                "method": "parakeet_python",
                "returncode": out.status.code(),
                "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string(),
                "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(),
            });
            if let Some(err) = server_error {
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert("server_fallback".to_string(), err);
                }
            }
            (String::new(), meta)
        }
        Err(e) => {
            let mut meta = json!({
                "status": "error",
                "method": "parakeet_python",
                "reason": format!("Failed to run python transcription: {e}"),
            });
            if let Some(err) = server_error {
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert("server_fallback".to_string(), err);
                }
            }
            (String::new(), meta)
        }
    }
}

pub(crate) fn process_manual_chunk(
    state: &mut SessionState,
    cli: &Cli,
    reason: &str,
    forced_end_sec: Option<f64>,
) -> Result<Value, AppError> {
    let session_dir = PathBuf::from(&state.session_dir);
    let events_path = PathBuf::from(&state.events_path);
    let source_audio = PathBuf::from(&state.audio_path);
    if !source_audio.exists() {
        return Ok(json!({
            "status": "skipped",
            "reason": format!("Audio file not found: {}", source_audio.display()),
        }));
    }
    if !command_exists("ffmpeg") {
        return Ok(json!({
            "status": "error",
            "reason": "ffmpeg is required for chunking but was not found in PATH.",
        }));
    }

    let events = read_jsonl_values(&events_path);
    let chunk_id = next_transcript_chunk_id(&events);
    let start_sec = state.transcription_cursor_sec.max(0.0);
    let effective_end_sec = forced_end_sec.unwrap_or_else(|| audio_elapsed_sec(state));

    if effective_end_sec <= start_sec + 0.05 {
        append_session_event(
            &events_path,
            &json!({
                "ts": now_iso(),
                "type": "transcript_chunk",
                "id": chunk_id,
                "mode": "manual",
                "status": "skipped",
                "reason": "no_new_audio",
                "requested_reason": reason,
                "start_sec": round3(start_sec),
                "end_sec": round3(effective_end_sec),
            }),
        )?;
        return Ok(json!({
            "status": "skipped",
            "reason": "no_new_audio",
            "start_sec": round3(start_sec),
            "end_sec": round3(effective_end_sec),
            "chunk_id": chunk_id
        }));
    }

    let scratch_audio = session_dir.join(".chunk-manual.wav");
    let scratch_txt = session_dir.join(".chunk-manual.txt");
    let _ = fs::remove_file(&scratch_audio);
    let _ = fs::remove_file(&scratch_txt);

    if let Err(e) =
        extract_audio_segment(&source_audio, start_sec, effective_end_sec, &scratch_audio)
    {
        append_session_event(
            &events_path,
            &json!({
                "ts": now_iso(),
                "type": "transcript_chunk",
                "id": chunk_id,
                "mode": "manual",
                "status": "error",
                "reason": "segment_extract_failed",
                "requested_reason": reason,
                "start_sec": round3(start_sec),
                "end_sec": round3(effective_end_sec),
                "error": e.message,
            }),
        )?;
        return Ok(json!({
            "status": "error",
            "reason": "segment_extract_failed",
            "start_sec": round3(start_sec),
            "end_sec": round3(effective_end_sec),
            "chunk_id": chunk_id
        }));
    }

    let (chunk_text, transcribe_meta) = transcribe_chunk_audio(&scratch_audio, &scratch_txt, cli);
    let _ = fs::remove_file(&scratch_audio);
    let _ = fs::remove_file(&scratch_txt);
    let transcribe_status = transcribe_meta
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("error");

    if transcribe_status != "ok" {
        append_session_event(
            &events_path,
            &json!({
                "ts": now_iso(),
                "type": "transcript_chunk",
                "id": chunk_id,
                "mode": "manual",
                "status": "error",
                "reason": "transcribe_failed",
                "requested_reason": reason,
                "start_sec": round3(start_sec),
                "end_sec": round3(effective_end_sec),
                "transcription": transcribe_meta,
            }),
        )?;
        return Ok(json!({
            "status": "error",
            "reason": "transcribe_failed",
            "start_sec": round3(start_sec),
            "end_sec": round3(effective_end_sec),
            "chunk_id": chunk_id,
            "transcription": transcribe_meta
        }));
    }

    let trimmed = chunk_text.trim().to_string();
    let final_status = if trimmed.is_empty() { "skipped" } else { "ok" };
    if !trimmed.is_empty() {
        append_transcript_text(&session_dir, &trimmed)?;
    }
    state.transcription_cursor_sec = effective_end_sec.max(state.transcription_cursor_sec);

    append_session_event(
        &events_path,
        &json!({
            "ts": now_iso(),
            "type": "transcript_chunk",
            "id": chunk_id,
            "mode": "manual",
            "status": final_status,
            "reason": if final_status == "ok" { "manual_chunk" } else { "empty_transcript" },
            "requested_reason": reason,
            "start_sec": round3(start_sec),
            "end_sec": round3(effective_end_sec),
            "chars": trimmed.len(),
            "words": trimmed.split_whitespace().count(),
            "transcription": transcribe_meta,
        }),
    )?;

    Ok(json!({
        "status": final_status,
        "reason": reason,
        "start_sec": round3(start_sec),
        "end_sec": round3(effective_end_sec),
        "chunk_id": chunk_id,
        "chars": trimmed.len(),
        "words": trimmed.split_whitespace().count(),
        "transcription": transcribe_meta
    }))
}
