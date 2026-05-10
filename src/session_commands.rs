use crate::cli::{Cli, StartArgs, StopArgs};
use crate::error::{app_error, AppError};
use crate::history::read_jsonl_values;
use crate::models::SessionState;
use crate::paths::{
    active_state_file, audio_device_cache_file, ensure_dirs, last_session_file, sessions_dir,
};
use crate::reporting::{
    build_html_note, build_note, clipboard_from_events, generate_sessions_index_html,
    inject_annotation_markers, load_shots_for_session, max_clipboard_id, max_shot_id,
    shots_from_events,
};
use crate::screenshots::{detect_screenshot_dir, file_mtime_epoch, move_session_screenshots};
use crate::transcription::{
    ensure_parakeet_server, ensure_web_server, parakeet_server_enabled,
    resolve_parakeet_batch_size, resolve_parakeet_model, resolve_parakeet_script,
    resolve_python_bin, run_post_transcribe_command, run_transcription,
};
use crate::{
    append_jsonl, append_perf_event, build_record_cmd, capture_frontmost_app_meta,
    capture_process_stats, clear_active_state, command_exists, emit_json, get_audio_duration_sec,
    load_active_state, now_iso, pause_recorder_capture, play_event_sound, print_out, print_verbose,
    process_is_alive, read_json, recorder_error_looks_like_invalid_audio_device,
    resolve_audio_device, resolve_audio_device_uncached, resume_recorder_capture, round3,
    save_active_state, session_stamp, spawn_clipboard_watcher, spawn_recorder,
    spawn_transcription_watcher, stop_clipboard_watcher, stop_recorder, stop_transcription_watcher,
    unix_now, wait_for_transcription_watcher, write_json,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn command_source(cli_value: Option<&str>, env_key: &str) -> &'static str {
    if cli_value.map(str::trim).filter(|s| !s.is_empty()).is_some() {
        "cli"
    } else if env::var(env_key)
        .ok()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        "env"
    } else {
        "off"
    }
}

fn load_chunked_transcript(session_dir: &Path, events_path: &Path) -> (String, Value) {
    let transcript_path = session_dir.join("transcript.txt");
    let transcript_raw = fs::read_to_string(&transcript_path)
        .unwrap_or_default()
        .trim()
        .to_string();

    let events = read_jsonl_values(events_path);
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
            "method": "manual_chunked",
            "mode": chunk_mode,
            "chunks": chunk_count,
            "chunks_ok": ok_chunks,
            "chunks_skipped": skipped_chunks,
            "chunks_error": errored_chunks,
            "chunk_audio_sec": round3(chunk_seconds),
            "stopping_seen": stopping_seen,
            "stop_reason": if stop_reason.is_empty() { Value::Null } else { Value::String(stop_reason) }
        }),
    )
}

fn audio_elapsed_sec(state: &SessionState) -> f64 {
    get_audio_duration_sec(Path::new(&state.audio_path))
        .unwrap_or_else(|| (unix_now() - state.started_at_epoch).max(0.0))
}

fn next_transcript_chunk_id(events: &[Value]) -> usize {
    events
        .iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("transcript_chunk"))
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .max()
        .unwrap_or(0) as usize
        + 1
}

fn merge_manual_chunk_text(existing: &str, chunk: &str) -> String {
    let left = existing.trim();
    let right = chunk.trim();
    if right.is_empty() {
        return left.to_string();
    }
    if left.is_empty() {
        return right.to_string();
    }
    format!("{left}\n\n{right}")
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
    let batch_size = resolve_parakeet_batch_size();
    let cmd_for_log = format!(
        "{} {} --audio {} --out-txt {} --model {} --batch-size {}",
        python_bin,
        script_path.display(),
        chunk_audio.display(),
        chunk_out_txt.display(),
        model,
        batch_size
    );
    print_verbose(
        cli,
        format!("Running chunk transcription (one-shot): {cmd_for_log}"),
    );

    let mut server_error: Option<Value> = None;
    if parakeet_server_enabled() && command_exists("curl") {
        ensure_parakeet_server(&python_bin, &script_path, &model, cli, true);
        let base_url =
            env::var("RIFF_PARAKEET_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:8765".into());
        let payload = json!({
            "audio": chunk_audio,
            "out_txt": chunk_out_txt,
            "model": model,
            "batch_size": batch_size
        })
        .to_string();
        let server_out = Command::new("curl")
            .args([
                "-sS",
                "--fail",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "--data",
                &payload,
            ])
            .arg(format!("{}/transcribe", base_url.trim_end_matches('/')))
            .output();

        match server_out {
            Ok(out) if out.status.success() => {
                let body = String::from_utf8_lossy(&out.stdout).to_string();
                match serde_json::from_str::<Value>(&body) {
                    Ok(parsed) if parsed.get("ok").and_then(|v| v.as_bool()) == Some(true) => {
                        let txt = parsed
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if !txt.is_empty() && !chunk_out_txt.exists() {
                            let _ = fs::write(chunk_out_txt, format!("{txt}\n"));
                        }
                        return (
                            txt,
                            json!({
                                "status": "ok",
                                "method": "parakeet_server",
                                "server": base_url,
                                "model": model,
                                "batch_size": batch_size,
                                "elapsed_sec": parsed.get("elapsed_sec").and_then(|v| v.as_f64()),
                            }),
                        );
                    }
                    Ok(parsed) => {
                        server_error = Some(json!({
                            "status": "error",
                            "method": "parakeet_server",
                            "server": base_url,
                            "response": parsed,
                        }));
                    }
                    Err(e) => {
                        server_error = Some(json!({
                            "status": "error",
                            "method": "parakeet_server",
                            "server": base_url,
                            "reason": format!("invalid JSON response: {e}"),
                            "body": body,
                        }));
                    }
                }
            }
            Ok(out) => {
                server_error = Some(json!({
                    "status": "error",
                    "method": "parakeet_server",
                    "server": base_url,
                    "returncode": out.status.code(),
                    "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string(),
                    "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(),
                }));
            }
            Err(e) => {
                server_error = Some(json!({
                    "status": "error",
                    "method": "parakeet_server",
                    "reason": format!("curl failed: {e}"),
                }));
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
        .arg("--batch-size")
        .arg(batch_size.to_string())
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
                "batch_size": batch_size,
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

fn process_manual_chunk(
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
        append_jsonl(
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
        append_jsonl(
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
        append_jsonl(
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

    let transcript_path = session_dir.join("transcript.txt");
    let trimmed = chunk_text.trim().to_string();
    let final_status = if trimmed.is_empty() { "skipped" } else { "ok" };
    if !trimmed.is_empty() {
        let existing = fs::read_to_string(&transcript_path).unwrap_or_default();
        let merged = merge_manual_chunk_text(&existing, &trimmed);
        fs::write(&transcript_path, format!("{merged}\n")).map_err(|e| {
            app_error(
                1,
                format!(
                    "Failed to write merged transcript {}: {e}",
                    transcript_path.display()
                ),
            )
        })?;
    }
    state.transcription_cursor_sec = effective_end_sec.max(state.transcription_cursor_sec);

    append_jsonl(
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

pub(crate) fn cmd_start(cli: &Cli, args: &StartArgs) -> Result<i32, AppError> {
    ensure_dirs()?;
    let perf_total = Instant::now();
    let mut stale_state_cleanup_ms = 0.0;
    let create_session_dir_ms: f64;
    let write_session_started_event_ms: f64;
    let ffmpeg_check_ms: f64;
    let initial_state_save_ms: f64;
    let clipboard_bootstrap_read_ms: f64;
    let clipboard_watcher_spawn_ms: f64;
    let mut clipboard_state_save_ms = 0.0;
    let transcription_watcher_spawn_ms: f64;
    let mut transcription_state_save_ms = 0.0;
    let mut parakeet_server_warmup_ms = 0.0;
    let mut parakeet_server_warmup_attempted = false;

    let active_path = active_state_file();
    if active_path.exists() {
        let t_stale_cleanup = Instant::now();
        let existing: SessionState = read_json(&active_path)?;
        if let Some(pid) = existing.clipboard_watcher_pid {
            stop_clipboard_watcher(pid, cli);
        }
        if let Some(pid) = existing.transcription_watcher_pid {
            stop_transcription_watcher(pid, cli);
        }
        if let Some(pid) = existing.ffmpeg_pid {
            if process_is_alive(pid) {
                return Err(app_error(
                    2,
                    format!(
                        "A session is already active (session_id={}, pid={}).",
                        existing.session_id, pid
                    ),
                ));
            }
        }
        print_verbose(cli, "Found stale active session state; removing.");
        fs::remove_file(&active_path)
            .map_err(|e| app_error(1, format!("Failed to remove stale state: {e}")))?;
        stale_state_cleanup_ms = elapsed_ms(t_stale_cleanup);
    }

    let t_screenshot_dir = Instant::now();
    let screenshot_dir = detect_screenshot_dir(args.screenshot_dir.as_deref(), cli)?;
    let screenshot_dir_ms = elapsed_ms(t_screenshot_dir);

    let session_id = session_stamp();
    let session_dir = sessions_dir().join(&session_id);
    let screenshots_dir = session_dir.join("screenshots");
    let audio_path = session_dir.join("audio.wav");
    let events_path = session_dir.join("events.jsonl");
    let ffmpeg_log_path = session_dir.join("ffmpeg.log");
    let started_epoch = unix_now();
    let started_iso = now_iso();

    let requested_audio_device = if args.audio_device.eq_ignore_ascii_case("auto") {
        env::var("RIFF_AUDIO_DEVICE").unwrap_or_else(|_| args.audio_device.clone())
    } else {
        args.audio_device.clone()
    };

    let t_audio_device = Instant::now();
    let mut resolved_audio_device = resolve_audio_device(&requested_audio_device, cli);
    let audio_device_ms = elapsed_ms(t_audio_device);

    if cli.dry_run {
        print_out(
            cli,
            "[dry-run] Would create session directory and start ffmpeg audio capture.",
        );
        print_out(
            cli,
            format!(
                "[dry-run] Planned session {}\nsession_dir: {}\naudio_path: {}\nscreenshot_source_dir: {}\naudio_device: {}",
                session_id,
                session_dir.display(),
                audio_path.display(),
                screenshot_dir.display(),
                resolved_audio_device,
            ),
        );
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "start",
                "session_id": session_id,
                "session_dir": session_dir,
                "audio_path": audio_path,
                "screenshot_source_dir": screenshot_dir,
                "audio_device": resolved_audio_device,
                "ffmpeg_pid": Value::Null,
                "dry_run": true,
                "state_saved": false
            }),
        );
        return Ok(0);
    }

    let t_create_session_dir = Instant::now();
    fs::create_dir_all(&screenshots_dir).map_err(|e| {
        app_error(
            1,
            format!("Failed to create {}: {e}", screenshots_dir.display()),
        )
    })?;
    create_session_dir_ms = elapsed_ms(t_create_session_dir);

    let t_write_session_started_event = Instant::now();
    append_jsonl(
        &events_path,
        &json!({
            "ts": started_iso,
            "type": "session_started",
            "session_id": session_id,
            "screenshot_source_dir": screenshot_dir,
        }),
    )?;
    write_session_started_event_ms = elapsed_ms(t_write_session_started_event);

    let t_ffmpeg_check = Instant::now();
    if !command_exists("ffmpeg") {
        return Err(app_error(
            5,
            "ffmpeg is required but was not found in PATH.",
        ));
    }
    ffmpeg_check_ms = elapsed_ms(t_ffmpeg_check);

    let t_spawn_recorder = Instant::now();
    let mut audio_device_retry = false;

    let ffmpeg_pid = {
        let record_cmd = build_record_cmd(&audio_path, &resolved_audio_device);
        match spawn_recorder(&record_cmd, &ffmpeg_log_path, cli) {
            Ok(pid) => pid,
            Err(first_err)
                if args.audio_device.eq_ignore_ascii_case("auto")
                    && recorder_error_looks_like_invalid_audio_device(&first_err) =>
            {
                print_verbose(
                    cli,
                    "Detected invalid audio device from cached/env selection; retrying with fresh auto-detect.",
                );
                let _ = fs::remove_file(audio_device_cache_file());

                let retry_device = resolve_audio_device_uncached(cli);
                let retry_cmd = build_record_cmd(&audio_path, &retry_device);

                match spawn_recorder(&retry_cmd, &ffmpeg_log_path, cli) {
                    Ok(pid) => {
                        resolved_audio_device = retry_device;
                        audio_device_retry = true;
                        pid
                    }
                    Err(second_err) => {
                        return Err(app_error(
                            second_err.code,
                            format!(
                                "Recorder failed with initial device selection and retry.\ninitial_error: {}\nretry_error: {}",
                                first_err.message, second_err.message
                            ),
                        ));
                    }
                }
            }
            Err(err) => return Err(err),
        }
    };

    let spawn_recorder_ms = elapsed_ms(t_spawn_recorder);

    let mut state = SessionState {
        session_id: session_id.clone(),
        session_dir: session_dir.display().to_string(),
        screenshots_dir: screenshots_dir.display().to_string(),
        audio_path: audio_path.display().to_string(),
        events_path: events_path.display().to_string(),
        ffmpeg_log_path: ffmpeg_log_path.display().to_string(),
        ffmpeg_pid: Some(ffmpeg_pid),
        started_at_iso: started_iso,
        started_at_epoch: started_epoch,
        screenshot_source_dir: screenshot_dir.display().to_string(),
        audio_device: resolved_audio_device.clone(),
        clipboard_watcher_pid: None,
        transcription_watcher_pid: None,
        transcription_cursor_sec: 0.0,
        transcription_paused: false,
        transcription_pause_started_sec: None,
    };

    let t_initial_state_save = Instant::now();
    save_active_state(&state)?;
    initial_state_save_ms = elapsed_ms(t_initial_state_save);

    let t_clipboard_bootstrap_read = Instant::now();
    let existing_clips = clipboard_from_events(&read_jsonl_values(&events_path));
    clipboard_bootstrap_read_ms = elapsed_ms(t_clipboard_bootstrap_read);

    let t_clipboard_watcher_spawn = Instant::now();
    clipboard_watcher_spawn_ms = if let Some(watcher_pid) =
        spawn_clipboard_watcher(&state, max_clipboard_id(&existing_clips), cli)
    {
        let ms = elapsed_ms(t_clipboard_watcher_spawn);
        state.clipboard_watcher_pid = Some(watcher_pid);
        let t_clipboard_state_save = Instant::now();
        save_active_state(&state)?;
        clipboard_state_save_ms = elapsed_ms(t_clipboard_state_save);
        ms
    } else {
        elapsed_ms(t_clipboard_watcher_spawn)
    };

    let mut transcription_watcher_spawned = false;
    let t_transcription_watcher_spawn = Instant::now();
    transcription_watcher_spawn_ms = if let Some(pid) = spawn_transcription_watcher(&state, cli) {
        let ms = elapsed_ms(t_transcription_watcher_spawn);
        state.transcription_watcher_pid = Some(pid);
        transcription_watcher_spawned = true;
        let t_transcription_state_save = Instant::now();
        save_active_state(&state)?;
        transcription_state_save_ms = elapsed_ms(t_transcription_state_save);
        ms
    } else {
        elapsed_ms(t_transcription_watcher_spawn)
    };
    let watcher_setup_ms = round3(
        initial_state_save_ms
            + clipboard_bootstrap_read_ms
            + clipboard_watcher_spawn_ms
            + clipboard_state_save_ms
            + transcription_watcher_spawn_ms
            + transcription_state_save_ms,
    );

    let custom_transcribe_enabled = env::var("RIFF_TRANSCRIBE_CMD")
        .ok()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !custom_transcribe_enabled && parakeet_server_enabled() {
        if let Some(script_path) = resolve_parakeet_script(None) {
            let t_parakeet_server_warmup = Instant::now();
            let python_bin = resolve_python_bin(None);
            let model = resolve_parakeet_model(None);
            ensure_parakeet_server(&python_bin, &script_path, &model, cli, false);
            parakeet_server_warmup_attempted = true;
            parakeet_server_warmup_ms = elapsed_ms(t_parakeet_server_warmup);
        }
    }

    let start_total_ms = elapsed_ms(perf_total);
    append_perf_event(json!({
        "ts": now_iso(),
        "action": "start",
        "session_id": session_id,
        "total_ms": round3(start_total_ms),
        "phases": {
            "stale_state_cleanup_ms": round3(stale_state_cleanup_ms),
            "detect_screenshot_dir_ms": round3(screenshot_dir_ms),
            "resolve_audio_device_ms": round3(audio_device_ms),
            "create_session_dir_ms": round3(create_session_dir_ms),
            "write_session_started_event_ms": round3(write_session_started_event_ms),
            "ffmpeg_check_ms": round3(ffmpeg_check_ms),
            "spawn_recorder_ms": round3(spawn_recorder_ms),
            "initial_state_save_ms": round3(initial_state_save_ms),
            "clipboard_bootstrap_read_ms": round3(clipboard_bootstrap_read_ms),
            "clipboard_watcher_spawn_ms": round3(clipboard_watcher_spawn_ms),
            "clipboard_state_save_ms": round3(clipboard_state_save_ms),
            "transcription_watcher_spawn_ms": round3(transcription_watcher_spawn_ms),
            "transcription_state_save_ms": round3(transcription_state_save_ms),
            "parakeet_server_warmup_ms": round3(parakeet_server_warmup_ms),
            "watcher_setup_ms": watcher_setup_ms
        },
        "audio_device_retry": audio_device_retry,
        "transcription_watcher_spawned": transcription_watcher_spawned,
        "transcription_watcher_pid": state.transcription_watcher_pid,
        "parakeet_server_warmup_attempted": parakeet_server_warmup_attempted
    }));

    play_event_sound("start", cli);

    print_out(
        cli,
        format!(
            "Started session {}\nsession_dir: {}\naudio_path: {}\nscreenshot_source_dir: {}\naudio_device: {}\naudio_device_retry: {}\nstartup_ms: {}\nstartup_phase_ms: spawn_recorder={} watcher_setup={} parakeet_server_warmup={} audio_device={} screenshot_dir={}",
            session_id,
            session_dir.display(),
            audio_path.display(),
            screenshot_dir.display(),
            resolved_audio_device,
            audio_device_retry,
            round3(start_total_ms),
            round3(spawn_recorder_ms),
            watcher_setup_ms,
            round3(parakeet_server_warmup_ms),
            round3(audio_device_ms),
            round3(screenshot_dir_ms),
        ),
    );

    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "start",
            "session_id": session_id,
            "session_dir": session_dir,
            "audio_path": audio_path,
            "screenshot_source_dir": screenshot_dir,
            "audio_device": resolved_audio_device,
            "audio_device_retry": audio_device_retry,
            "ffmpeg_pid": ffmpeg_pid,
            "transcription_watcher_pid": state.transcription_watcher_pid,
            "startup_ms": round3(start_total_ms),
            "phases": {
                "stale_state_cleanup_ms": round3(stale_state_cleanup_ms),
                "detect_screenshot_dir_ms": round3(screenshot_dir_ms),
                "resolve_audio_device_ms": round3(audio_device_ms),
                "create_session_dir_ms": round3(create_session_dir_ms),
                "write_session_started_event_ms": round3(write_session_started_event_ms),
                "ffmpeg_check_ms": round3(ffmpeg_check_ms),
                "spawn_recorder_ms": round3(spawn_recorder_ms),
                "initial_state_save_ms": round3(initial_state_save_ms),
                "clipboard_bootstrap_read_ms": round3(clipboard_bootstrap_read_ms),
                "clipboard_watcher_spawn_ms": round3(clipboard_watcher_spawn_ms),
                "clipboard_state_save_ms": round3(clipboard_state_save_ms),
                "transcription_watcher_spawn_ms": round3(transcription_watcher_spawn_ms),
                "transcription_state_save_ms": round3(transcription_state_save_ms),
                "parakeet_server_warmup_ms": round3(parakeet_server_warmup_ms),
                "watcher_setup_ms": watcher_setup_ms
            },
            "parakeet_server_warmup_attempted": parakeet_server_warmup_attempted,
            "dry_run": false,
            "state_saved": true
        }),
    );

    Ok(0)
}

pub(crate) fn cmd_shot(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;
    let state = load_active_state()?;

    let screenshots_dir = PathBuf::from(&state.screenshots_dir);
    let events_path = PathBuf::from(&state.events_path);

    fs::create_dir_all(&screenshots_dir).map_err(|e| {
        app_error(
            1,
            format!("Failed to create {}: {e}", screenshots_dir.display()),
        )
    })?;

    let events = read_jsonl_values(&events_path);
    let existing_shots = shots_from_events(&events);
    let shot_id = max_shot_id(&existing_shots) + 1;

    let dest_name = format!("shot-{shot_id:03}.png");
    let dest_rel = format!("screenshots/{dest_name}");
    let dest_abs = screenshots_dir.join(&dest_name);

    if cli.dry_run {
        print_out(
            cli,
            format!(
                "[dry-run] Would capture screenshot to {}",
                dest_abs.display()
            ),
        );
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "shot",
                "session_id": state.session_id,
                "id": shot_id,
                "dest": dest_abs,
                "dry_run": true,
            }),
        );
        return Ok(0);
    }

    let status = Command::new("screencapture")
        .args(["-i", "-x"])
        .arg(&dest_abs)
        .status()
        .map_err(|e| app_error(1, format!("Failed to run screencapture: {e}")))?;

    if !status.success() {
        if status.code() == Some(1) {
            return Err(app_error(9, "Screenshot canceled."));
        }
        return Err(app_error(
            1,
            format!("screencapture failed with status: {status}"),
        ));
    }

    if !dest_abs.exists() {
        return Err(app_error(
            1,
            format!("Screenshot did not produce file: {}", dest_abs.display()),
        ));
    }

    let mtime = file_mtime_epoch(&dest_abs).unwrap_or_else(unix_now);
    let audio_sec = (mtime - state.started_at_epoch).max(0.0);
    let (app_payload, app_capture_error, app_pid) = match capture_frontmost_app_meta(cli) {
        Ok(meta) => (
            json!({
                "name": meta.name,
                "bundle_id": meta.bundle_id,
                "pid": meta.pid,
                "window_title": meta.window_title,
            }),
            Value::Null,
            meta.pid,
        ),
        Err(reason) => (Value::Null, Value::String(reason), None),
    };
    let (process_payload, process_capture_error) = match app_pid {
        Some(pid) => match capture_process_stats(pid, cli) {
            Ok(proc_stats) => (
                json!({
                    "cpu_percent": proc_stats.cpu_percent,
                    "mem_percent": proc_stats.mem_percent,
                    "rss_kb": proc_stats.rss_kb,
                    "elapsed": proc_stats.elapsed,
                    "state": proc_stats.state,
                    "command": proc_stats.command,
                }),
                Value::Null,
            ),
            Err(reason) => (Value::Null, Value::String(reason)),
        },
        None => (
            Value::Null,
            Value::String("app_pid_unavailable".to_string()),
        ),
    };

    append_jsonl(
        &events_path,
        &json!({
            "ts": now_iso(),
            "type": "screenshot_taken",
            "id": shot_id,
            "dest": dest_rel,
            "dest_abs": dest_abs,
            "audioSec": round3(audio_sec),
            "mtime_epoch": round3(mtime),
            "method": "direct_screencapture",
            "app": app_payload,
            "app_capture_error": app_capture_error,
            "process": process_payload,
            "process_capture_error": process_capture_error,
        }),
    )?;

    print_out(
        cli,
        format!(
            "Captured screenshot {}\npath: {}",
            shot_id,
            dest_abs.display()
        ),
    );

    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "shot",
            "session_id": state.session_id,
            "id": shot_id,
            "dest": dest_abs,
            "dest_rel": dest_rel,
            "audioSec": round3(audio_sec),
            "app": app_payload,
            "app_capture_error": app_capture_error,
            "process": process_payload,
            "process_capture_error": process_capture_error,
            "dry_run": false,
        }),
    );

    Ok(0)
}

pub(crate) fn cmd_chunk(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;
    if !active_state_file().exists() {
        print_out(cli, "No active session.");
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "chunk",
                "active": false,
                "message": "No active session."
            }),
        );
        return Ok(0);
    }

    let mut state = load_active_state()?;
    if cli.dry_run {
        let now_sec = audio_elapsed_sec(&state);
        print_out(
            cli,
            format!(
                "[dry-run] Would transcribe chunk from {:.3}s to {:.3}s",
                state.transcription_cursor_sec, now_sec
            ),
        );
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "chunk",
                "session_id": state.session_id,
                "start_sec": round3(state.transcription_cursor_sec),
                "end_sec": round3(now_sec),
                "dry_run": true
            }),
        );
        return Ok(0);
    }

    let result = process_manual_chunk(&mut state, cli, "manual", None)?;
    save_active_state(&state)?;

    let status = result
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let start_sec = result
        .get("start_sec")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let end_sec = result
        .get("end_sec")
        .and_then(|v| v.as_f64())
        .unwrap_or(start_sec);
    let words = result.get("words").and_then(|v| v.as_u64()).unwrap_or(0);
    print_out(
        cli,
        format!(
            "Chunk {} [{}]: {:.3}s -> {:.3}s ({} words)",
            result.get("chunk_id").and_then(|v| v.as_u64()).unwrap_or(0),
            status,
            start_sec,
            end_sec,
            words
        ),
    );

    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "chunk",
            "session_id": state.session_id,
            "chunk": result
        }),
    );
    Ok(0)
}

pub(crate) fn cmd_pause(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;
    if !active_state_file().exists() {
        print_out(cli, "No active session.");
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "pause",
                "active": false,
                "message": "No active session."
            }),
        );
        return Ok(0);
    }

    let mut state = load_active_state()?;
    if state.transcription_paused {
        print_out(cli, "Already paused.");
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "pause",
                "session_id": state.session_id,
                "already_paused": true
            }),
        );
        return Ok(0);
    }

    let Some(ffmpeg_pid) = state.ffmpeg_pid else {
        return Err(app_error(
            1,
            "Active session has no recorder pid; cannot pause.",
        ));
    };
    if !process_is_alive(ffmpeg_pid) {
        return Err(app_error(
            1,
            format!("Recorder pid={ffmpeg_pid} is not alive; cannot pause."),
        ));
    }

    if !cli.dry_run {
        pause_recorder_capture(ffmpeg_pid, cli)?;
        thread::sleep(Duration::from_millis(60));
    }
    let pause_at_sec = audio_elapsed_sec(&state);
    let paused_at_epoch = unix_now();
    let flush = if cli.dry_run {
        json!({
            "status": "dry_run",
            "start_sec": round3(state.transcription_cursor_sec),
            "end_sec": round3(pause_at_sec)
        })
    } else {
        process_manual_chunk(&mut state, cli, "pause_flush", Some(pause_at_sec))?
    };

    state.transcription_paused = true;
    state.transcription_pause_started_sec = Some(paused_at_epoch);
    if !cli.dry_run {
        append_jsonl(
            Path::new(&state.events_path),
            &json!({
                "ts": now_iso(),
                "type": "session_paused",
                "session_id": state.session_id,
                "audioSec": round3(pause_at_sec),
                "cursor_sec": round3(state.transcription_cursor_sec),
                "ffmpeg_pid": ffmpeg_pid,
                "paused_at_epoch": round3(paused_at_epoch),
            }),
        )?;
        save_active_state(&state)?;
    }

    print_out(
        cli,
        format!(
            "Paused recording at {:.3}s (cursor {:.3}s)",
            pause_at_sec, state.transcription_cursor_sec
        ),
    );
    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "pause",
            "session_id": state.session_id,
            "paused": true,
            "pause_at_sec": round3(pause_at_sec),
            "cursor_sec": round3(state.transcription_cursor_sec),
            "ffmpeg_pid": ffmpeg_pid,
            "flush": flush,
            "dry_run": cli.dry_run
        }),
    );
    Ok(0)
}

pub(crate) fn cmd_unpause(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;
    if !active_state_file().exists() {
        print_out(cli, "No active session.");
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "unpause",
                "active": false,
                "message": "No active session."
            }),
        );
        return Ok(0);
    }

    let mut state = load_active_state()?;
    if !state.transcription_paused {
        print_out(cli, "Not paused.");
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "unpause",
                "session_id": state.session_id,
                "was_paused": false
            }),
        );
        return Ok(0);
    }

    let Some(ffmpeg_pid) = state.ffmpeg_pid else {
        return Err(app_error(
            1,
            "Active session has no recorder pid; cannot unpause.",
        ));
    };
    if !process_is_alive(ffmpeg_pid) {
        return Err(app_error(
            1,
            format!("Recorder pid={ffmpeg_pid} is not alive; cannot unpause."),
        ));
    }

    let pause_started_sec = state
        .transcription_pause_started_sec
        .unwrap_or_else(unix_now);
    if !cli.dry_run {
        resume_recorder_capture(ffmpeg_pid, cli)?;
    }
    let unpause_at_sec = audio_elapsed_sec(&state);
    let paused_sec = (unix_now() - pause_started_sec).max(0.0);
    state.transcription_paused = false;
    state.transcription_pause_started_sec = None;

    if !cli.dry_run {
        append_jsonl(
            Path::new(&state.events_path),
            &json!({
                "ts": now_iso(),
                "type": "session_unpaused",
                "session_id": state.session_id,
                "audioSec": round3(unpause_at_sec),
                "paused_sec": round3(paused_sec),
                "cursor_sec": round3(state.transcription_cursor_sec),
                "ffmpeg_pid": ffmpeg_pid,
            }),
        )?;
        save_active_state(&state)?;
    }

    print_out(
        cli,
        format!(
            "Resumed recording at {:.3}s (paused {:.3}s wall-clock)",
            unpause_at_sec, paused_sec
        ),
    );
    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "unpause",
            "session_id": state.session_id,
            "paused": false,
            "unpause_at_sec": round3(unpause_at_sec),
            "paused_sec": round3(paused_sec),
            "cursor_sec": round3(state.transcription_cursor_sec),
            "ffmpeg_pid": ffmpeg_pid,
            "dry_run": cli.dry_run
        }),
    );
    Ok(0)
}

pub(crate) fn cmd_stop(cli: &Cli, args: &StopArgs) -> Result<i32, AppError> {
    ensure_dirs()?;
    if !active_state_file().exists() {
        print_out(cli, "No active session.");
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "stop",
                "active": false,
                "message": "No active session."
            }),
        );
        return Ok(0);
    }

    let perf_total = Instant::now();
    let mut state = load_active_state()?;

    let session_dir = PathBuf::from(&state.session_dir);
    let screenshots_dir = PathBuf::from(&state.screenshots_dir);
    let events_path = PathBuf::from(&state.events_path);

    let ended_epoch = unix_now();
    let ended_iso = now_iso();

    let mut stop_recorder_ms = 0.0;
    let mut write_ms = 0.0;
    let mut web_server_ms = 0.0;
    let mut write_note_html_ms = 0.0;
    let mut append_stop_event_ms = 0.0;
    let mut write_last_session_ms = 0.0;
    let mut generate_index_ms = 0.0;
    let mut clear_state_ms = 0.0;
    let mut transcription_wait_ms = 0.0;
    let mut stop_flush_ms = 0.0;
    let mut stop_clipboard_watcher_ms = 0.0;
    let mut append_stopping_event_ms = 0.0;
    let mut resume_before_stop_ms = 0.0;
    let mut transcription_watcher_stop_ms = 0.0;
    let source_dir_check_ms: f64;
    let load_prior_events_ms: f64;
    let load_existing_shots_ms: f64;
    let collect_clipboard_events_ms: f64;
    let audio_duration_ms: f64;
    let build_note_bundle_ms: f64;
    let post_transcribe_ms: f64;
    let render_ms: f64;
    let mut write_note_md_ms = 0.0;
    let mut write_note_html_file_ms = 0.0;
    let mut transcription_forced_stop = false;
    let mut stop_flush_meta = Value::Null;
    let mut use_chunked_transcript = false;
    let custom_transcribe_source =
        command_source(args.transcribe_cmd.as_deref(), "RIFF_TRANSCRIBE_CMD");
    let post_transcribe_source = command_source(
        args.post_transcribe_cmd.as_deref(),
        "RIFF_POST_TRANSCRIBE_CMD",
    );
    let use_custom_transcribe = custom_transcribe_source != "off";

    print_verbose(
        cli,
        format!(
            "Stop pipeline: session_id={} watcher_pid={:?} cursor_sec={:.3} paused={} transcribe_cmd={} post_transcribe_cmd={}",
            state.session_id,
            state.transcription_watcher_pid,
            state.transcription_cursor_sec,
            state.transcription_paused,
            custom_transcribe_source,
            post_transcribe_source,
        ),
    );

    if !cli.dry_run {
        if let Some(pid) = state.clipboard_watcher_pid {
            let t_stop_clipboard_watcher = Instant::now();
            stop_clipboard_watcher(pid, cli);
            stop_clipboard_watcher_ms = elapsed_ms(t_stop_clipboard_watcher);
        }
        let t_append_stopping_event = Instant::now();
        append_jsonl(
            &events_path,
            &json!({
                "ts": ended_iso,
                "type": "session_stopping",
                "session_id": state.session_id,
            }),
        )?;
        append_stopping_event_ms = elapsed_ms(t_append_stopping_event);
    }

    if !cli.dry_run {
        let t_stop_recorder = Instant::now();
        if let Some(pid) = state.ffmpeg_pid {
            if state.transcription_paused && process_is_alive(pid) {
                let t_resume_before_stop = Instant::now();
                let _ = resume_recorder_capture(pid, cli);
                thread::sleep(Duration::from_millis(20));
                resume_before_stop_ms = elapsed_ms(t_resume_before_stop);
            }
            stop_recorder(pid, cli)?;
        }
        stop_recorder_ms = elapsed_ms(t_stop_recorder);

        if let Some(pid) = state.transcription_watcher_pid {
            let t_transcription_watcher_stop = Instant::now();
            print_verbose(
                cli,
                format!("Waiting up to 12s for transcription watcher pid={pid} to finish."),
            );
            let (finished, waited_ms) =
                wait_for_transcription_watcher(pid, Duration::from_secs(12), cli);
            transcription_wait_ms = waited_ms;
            if !finished {
                transcription_forced_stop = true;
                print_verbose(
                    cli,
                    format!(
                        "Transcription watcher pid={pid} did not finish in time; forcing stop."
                    ),
                );
                stop_transcription_watcher(pid, cli);
            }
            transcription_watcher_stop_ms = elapsed_ms(t_transcription_watcher_stop);
        }

        if !use_custom_transcribe {
            let should_flush_manual_chunk = state.transcription_watcher_pid.is_some()
                || state.transcription_cursor_sec > 0.05
                || state.transcription_paused;
            if should_flush_manual_chunk {
                use_chunked_transcript = true;
                print_verbose(
                    cli,
                    format!(
                        "Stop transcription strategy: chunked_flush (watcher_pid={:?} cursor_sec={:.3} paused={})",
                        state.transcription_watcher_pid,
                        state.transcription_cursor_sec,
                        state.transcription_paused
                    ),
                );
                let t_stop_flush = Instant::now();
                stop_flush_meta = match process_manual_chunk(&mut state, cli, "stop_flush", None) {
                    Ok(meta) => meta,
                    Err(e) => json!({
                        "status": "error",
                        "reason": e.message,
                    }),
                };
                stop_flush_ms = elapsed_ms(t_stop_flush);
                print_verbose(cli, format!("Stop flush result: {}", stop_flush_meta));
            } else {
                print_verbose(cli, "Stop transcription strategy: full_transcribe_on_stop");
                stop_flush_meta = json!({
                    "status": "skipped",
                    "reason": "full_transcribe_on_stop",
                });
            }
        }
        state.transcription_paused = false;
        state.transcription_pause_started_sec = None;
    }

    let source_dir = PathBuf::from(&state.screenshot_source_dir);
    let t_source_dir_check = Instant::now();
    if !source_dir.is_dir() {
        return Err(app_error(
            7,
            format!(
                "Screenshot source directory missing: {}",
                source_dir.display()
            ),
        ));
    }
    source_dir_check_ms = elapsed_ms(t_source_dir_check);

    let t_move_screens = Instant::now();
    let t_load_prior_events = Instant::now();
    let prior_events = read_jsonl_values(&events_path);
    load_prior_events_ms = elapsed_ms(t_load_prior_events);
    let t_load_existing_shots = Instant::now();
    let mut shots = load_shots_for_session(&session_dir, &prior_events);
    load_existing_shots_ms = elapsed_ms(t_load_existing_shots);
    let t_collect_clipboard_events = Instant::now();
    let clips = clipboard_from_events(&prior_events);
    collect_clipboard_events_ms = elapsed_ms(t_collect_clipboard_events);
    let moved_shots = move_session_screenshots(
        &source_dir,
        &screenshots_dir,
        state.started_at_epoch,
        ended_epoch,
        &events_path,
        max_shot_id(&shots),
        cli,
    )?;
    let moved_count = moved_shots.len();
    shots.extend(moved_shots);
    shots.sort_by_key(|s| s.shot_id);
    let move_screenshots_ms = elapsed_ms(t_move_screens);

    let t_transcribe = Instant::now();
    let (transcript_raw, mut transcription_meta) = if use_custom_transcribe {
        run_transcription(&state, &session_dir, args, cli)
    } else if use_chunked_transcript {
        load_chunked_transcript(&session_dir, &events_path)
    } else {
        run_transcription(&state, &session_dir, args, cli)
    };
    if !use_custom_transcribe && use_chunked_transcript {
        if let Some(obj) = transcription_meta.as_object_mut() {
            obj.insert(
                "forced_watcher_stop".to_string(),
                json!(transcription_forced_stop),
            );
            obj.insert(
                "watcher_wait_ms".to_string(),
                json!(round3(transcription_wait_ms)),
            );
            obj.insert("stop_flush".to_string(), stop_flush_meta.clone());
        }
    }
    let transcribe_ms = elapsed_ms(t_transcribe);
    print_verbose(
        cli,
        format!(
            "Transcription result: status={} method={} chars={} words={} elapsed_ms={}",
            transcription_meta
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            transcription_meta
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            transcript_raw.chars().count(),
            transcript_raw.split_whitespace().count(),
            round3(transcribe_ms),
        ),
    );

    let t_post_transcribe = Instant::now();
    let pre_post_chars = transcript_raw.chars().count();
    let (transcript_raw, post_transcribe_meta) =
        if transcription_meta.get("status").and_then(|v| v.as_str()) == Some("ok") {
            run_post_transcribe_command(&transcript_raw, &state, &session_dir, args, cli)
        } else {
            (
                transcript_raw,
                json!({"status": "skipped", "reason": "transcription_not_ok"}),
            )
        };
    post_transcribe_ms = elapsed_ms(t_post_transcribe);
    print_verbose(
        cli,
        format!(
            "Post-transcribe hook: status={} source={} chars_before={} chars_after={} elapsed_ms={}",
            post_transcribe_meta
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            post_transcribe_source,
            pre_post_chars,
            transcript_raw.chars().count(),
            round3(post_transcribe_ms),
        ),
    );
    if let Some(obj) = transcription_meta.as_object_mut() {
        obj.insert("post_process".to_string(), post_transcribe_meta.clone());
        if post_transcribe_meta.get("status").and_then(|v| v.as_str()) == Some("error") {
            obj.insert("status".to_string(), json!("error"));
            obj.insert("reason".to_string(), json!("post_transcribe_failed"));
        }
    }

    let t_audio_duration = Instant::now();
    let audio_duration = get_audio_duration_sec(Path::new(&state.audio_path));
    audio_duration_ms = elapsed_ms(t_audio_duration);

    let t_render = Instant::now();
    let transcript_annotated =
        inject_annotation_markers(&transcript_raw, &shots, &clips, audio_duration);
    let note_md = build_note(
        &state,
        &ended_iso,
        &shots,
        &clips,
        &transcript_annotated,
        &transcription_meta,
        audio_duration,
    );
    let note_path = session_dir.join("note.md");
    let html_body = build_html_note(
        &state.session_id,
        &state.started_at_iso,
        &ended_iso,
        audio_duration,
        &transcription_meta,
        &transcript_annotated,
        &note_md,
        &shots,
        &clips,
        &session_dir,
        "../index.html",
    );
    let html_path = session_dir.join("note.html");
    render_ms = elapsed_ms(t_render);
    build_note_bundle_ms = render_ms;

    if cli.dry_run {
        print_out(
            cli,
            format!("[dry-run] Would write note: {}", note_path.display()),
        );
        print_out(
            cli,
            format!("[dry-run] Would write html: {}", html_path.display()),
        );
    } else {
        let t_write = Instant::now();

        let t_write_note_md = Instant::now();
        fs::write(&note_path, format!("{}\n", note_md)).map_err(|e| {
            app_error(
                1,
                format!("Failed to write note {}: {e}", note_path.display()),
            )
        })?;
        write_note_md_ms = elapsed_ms(t_write_note_md);

        let t_write_note_html = Instant::now();
        fs::write(&html_path, html_body).map_err(|e| {
            app_error(
                1,
                format!("Failed to write HTML note {}: {e}", html_path.display()),
            )
        })?;
        write_note_html_file_ms = elapsed_ms(t_write_note_html);
        write_note_html_ms = round3(write_note_md_ms + write_note_html_file_ms);

        let t_append_stop_event = Instant::now();
        append_jsonl(
            &events_path,
            &json!({
                "ts": now_iso(),
                "type": "session_stopped",
                "session_id": state.session_id,
                "screenshots": shots.len(),
                "clipboard_captures": clips.len(),
                "screenshots_moved": moved_count,
                "audio_duration_sec": audio_duration,
                "note": note_path,
                "html": html_path,
                "transcription": transcription_meta,
            }),
        )?;
        append_stop_event_ms = elapsed_ms(t_append_stop_event);

        let t_last_session = Instant::now();
        write_json(
            &last_session_file(),
            &json!({
                "session_id": state.session_id,
                "session_dir": session_dir,
                "note_path": note_path,
                "html_path": html_path,
                "stopped_at": ended_iso,
                "screenshots": shots.len(),
                "clipboard_captures": clips.len(),
            }),
        )?;
        write_last_session_ms = elapsed_ms(t_last_session);

        let t_index = Instant::now();
        let _ = generate_sessions_index_html()?;
        generate_index_ms = elapsed_ms(t_index);

        let t_clear_state = Instant::now();
        clear_active_state()?;
        clear_state_ms = elapsed_ms(t_clear_state);

        write_ms = elapsed_ms(t_write);

        let t_web = Instant::now();
        let _ = ensure_web_server(cli, false);
        web_server_ms = elapsed_ms(t_web);
    }

    let stop_total_ms = elapsed_ms(perf_total);
    let transcription_perf = transcription_meta
        .get("perf")
        .cloned()
        .unwrap_or(Value::Null);
    append_perf_event(json!({
        "ts": now_iso(),
        "action": "stop",
        "session_id": state.session_id,
        "total_ms": round3(stop_total_ms),
        "phases": {
            "stop_clipboard_watcher_ms": round3(stop_clipboard_watcher_ms),
            "append_stopping_event_ms": round3(append_stopping_event_ms),
            "resume_before_stop_ms": round3(resume_before_stop_ms),
            "stop_recorder_ms": round3(stop_recorder_ms),
            "transcription_watcher_stop_ms": round3(transcription_watcher_stop_ms),
            "source_dir_check_ms": round3(source_dir_check_ms),
            "load_prior_events_ms": round3(load_prior_events_ms),
            "load_existing_shots_ms": round3(load_existing_shots_ms),
            "collect_clipboard_events_ms": round3(collect_clipboard_events_ms),
            "move_screenshots_ms": round3(move_screenshots_ms),
            "transcribe_ms": round3(transcribe_ms),
            "post_transcribe_ms": round3(post_transcribe_ms),
            "audio_duration_ms": round3(audio_duration_ms),
            "render_ms": round3(render_ms),
            "build_note_bundle_ms": round3(build_note_bundle_ms),
            "write_ms": round3(write_ms),
            "write_note_md_ms": round3(write_note_md_ms),
            "write_note_html_file_ms": round3(write_note_html_file_ms),
            "write_note_html_ms": round3(write_note_html_ms),
            "append_stop_event_ms": round3(append_stop_event_ms),
            "write_last_session_ms": round3(write_last_session_ms),
            "generate_index_ms": round3(generate_index_ms),
            "clear_state_ms": round3(clear_state_ms),
            "web_server_ms": round3(web_server_ms),
            "transcription_watcher_wait_ms": round3(transcription_wait_ms),
            "stop_flush_ms": round3(stop_flush_ms)
        },
        "transcription_perf": transcription_perf,
        "transcription_method": transcription_meta.get("method").and_then(|v| v.as_str()),
        "transcription_status": transcription_meta.get("status").and_then(|v| v.as_str())
    }));

    print_verbose(
        cli,
        format!(
            "Stop instrumentation summary: total_ms={} transcribe_ms={} post_transcribe_ms={} render_ms={} write_ms={}",
            round3(stop_total_ms),
            round3(transcribe_ms),
            round3(post_transcribe_ms),
            round3(render_ms),
            round3(write_ms),
        ),
    );

    if !cli.dry_run {
        play_event_sound("stop", cli);
    }

    print_out(
        cli,
        format!(
            "Stopped session {}\nsession_dir: {}\nscreenshots_moved: {}\nscreenshots_total: {}\nclipboard_captures: {}\nnote: {}\nhtml: {}\nstop_ms: {}\nstop_phase_ms: transcribe={} stop_recorder={} move_screenshots={} render={} write={}",
            state.session_id,
            session_dir.display(),
            moved_count,
            shots.len(),
            clips.len(),
            note_path.display(),
            html_path.display(),
            round3(stop_total_ms),
            round3(transcribe_ms),
            round3(stop_recorder_ms),
            round3(move_screenshots_ms),
            round3(render_ms),
            round3(write_ms),
        ),
    );

    if transcription_meta.get("status").and_then(|s| s.as_str()) != Some("ok") {
        let status = transcription_meta
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let reason = transcription_meta
            .get("reason")
            .and_then(|v| v.as_str())
            .or_else(|| transcription_meta.get("stderr").and_then(|v| v.as_str()))
            .unwrap_or("");
        print_out(cli, format!("transcription_status: {status} ({reason})"));
    }

    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "stop",
            "session_id": state.session_id,
            "session_dir": session_dir,
            "note_path": note_path,
            "html_path": html_path,
            "screenshots_moved": moved_count,
            "screenshots_total": shots.len(),
            "clipboard_captures": clips.len(),
            "stop_ms": round3(stop_total_ms),
            "phases": {
                "stop_clipboard_watcher_ms": round3(stop_clipboard_watcher_ms),
                "append_stopping_event_ms": round3(append_stopping_event_ms),
                "resume_before_stop_ms": round3(resume_before_stop_ms),
                "stop_recorder_ms": round3(stop_recorder_ms),
                "transcription_watcher_stop_ms": round3(transcription_watcher_stop_ms),
                "source_dir_check_ms": round3(source_dir_check_ms),
                "load_prior_events_ms": round3(load_prior_events_ms),
                "load_existing_shots_ms": round3(load_existing_shots_ms),
                "collect_clipboard_events_ms": round3(collect_clipboard_events_ms),
                "move_screenshots_ms": round3(move_screenshots_ms),
                "transcribe_ms": round3(transcribe_ms),
                "audio_duration_ms": round3(audio_duration_ms),
                "render_ms": round3(render_ms),
                "build_note_bundle_ms": round3(build_note_bundle_ms),
                "write_ms": round3(write_ms),
                "write_note_md_ms": round3(write_note_md_ms),
                "write_note_html_file_ms": round3(write_note_html_file_ms),
                "write_note_html_ms": round3(write_note_html_ms),
                "append_stop_event_ms": round3(append_stop_event_ms),
                "write_last_session_ms": round3(write_last_session_ms),
                "generate_index_ms": round3(generate_index_ms),
                "clear_state_ms": round3(clear_state_ms),
                "web_server_ms": round3(web_server_ms),
                "transcription_watcher_wait_ms": round3(transcription_wait_ms),
                "stop_flush_ms": round3(stop_flush_ms)
            },
            "transcription": transcription_meta,
            "transcription_watcher_forced_stop": transcription_forced_stop,
            "dry_run": cli.dry_run,
        }),
    );

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::merge_manual_chunk_text;

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
}
