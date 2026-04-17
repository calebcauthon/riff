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
use crate::transcription::{
    ensure_parakeet_server, ensure_web_server, parakeet_server_enabled, resolve_parakeet_model,
    resolve_parakeet_script, resolve_python_bin, run_transcription,
};
use crate::{
    append_jsonl, append_perf_event, build_record_cmd, capture_frontmost_app_meta,
    capture_process_stats, clear_active_state, command_exists, detect_screenshot_dir, emit_json,
    file_mtime_epoch, get_audio_duration_sec, load_active_state, move_session_screenshots, now_iso,
    play_event_sound, print_out, print_verbose, process_is_alive, read_json,
    recorder_error_looks_like_invalid_audio_device, resolve_audio_device,
    resolve_audio_device_uncached, round3, save_active_state, session_stamp,
    spawn_clipboard_watcher, spawn_recorder, stop_clipboard_watcher, stop_recorder, unix_now,
    write_json,
};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn cmd_start(cli: &Cli, args: &StartArgs) -> Result<i32, AppError> {
    ensure_dirs()?;
    let perf_total = Instant::now();

    let active_path = active_state_file();
    if active_path.exists() {
        let existing: SessionState = read_json(&active_path)?;
        if let Some(pid) = existing.clipboard_watcher_pid {
            stop_clipboard_watcher(pid, cli);
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
    }

    let t_screenshot_dir = Instant::now();
    let screenshot_dir = detect_screenshot_dir(args.screenshot_dir.as_deref(), cli)?;
    let screenshot_dir_ms = t_screenshot_dir.elapsed().as_secs_f64() * 1000.0;

    let session_id = session_stamp();
    let session_dir = sessions_dir().join(&session_id);
    let screenshots_dir = session_dir.join("screenshots");
    let audio_path = session_dir.join("audio.wav");
    let events_path = session_dir.join("events.jsonl");
    let ffmpeg_log_path = session_dir.join("ffmpeg.log");
    let started_epoch = unix_now();
    let started_iso = now_iso();

    let requested_audio_device = if args.audio_device.eq_ignore_ascii_case("auto") {
        env::var("ISPY_AUDIO_DEVICE").unwrap_or_else(|_| args.audio_device.clone())
    } else {
        args.audio_device.clone()
    };

    let t_audio_device = Instant::now();
    let mut resolved_audio_device = resolve_audio_device(&requested_audio_device, cli);
    let audio_device_ms = t_audio_device.elapsed().as_secs_f64() * 1000.0;

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

    fs::create_dir_all(&screenshots_dir).map_err(|e| {
        app_error(
            1,
            format!("Failed to create {}: {e}", screenshots_dir.display()),
        )
    })?;

    append_jsonl(
        &events_path,
        &json!({
            "ts": started_iso,
            "type": "session_started",
            "session_id": session_id,
            "screenshot_source_dir": screenshot_dir,
        }),
    )?;

    if !command_exists("ffmpeg") {
        return Err(app_error(
            5,
            "ffmpeg is required but was not found in PATH.",
        ));
    }

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

    let spawn_recorder_ms = t_spawn_recorder.elapsed().as_secs_f64() * 1000.0;

    let state = SessionState {
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
    };

    save_active_state(&state)?;

    let existing_clips = clipboard_from_events(&read_jsonl_values(&events_path));
    if let Some(watcher_pid) =
        spawn_clipboard_watcher(&state, max_clipboard_id(&existing_clips), cli)
    {
        let mut updated = state;
        updated.clipboard_watcher_pid = Some(watcher_pid);
        save_active_state(&updated)?;
    }

    let mut prewarm_ms = 0.0;
    // Warm the Parakeet server in the background so stop is faster.
    if parakeet_server_enabled() {
        let t_prewarm = Instant::now();
        if let Some(script_path) = resolve_parakeet_script(None) {
            let python_bin = resolve_python_bin(None);
            let model = resolve_parakeet_model(None);
            ensure_parakeet_server(&python_bin, &script_path, &model, cli, false);
        }
        prewarm_ms = t_prewarm.elapsed().as_secs_f64() * 1000.0;
    }

    let start_total_ms = perf_total.elapsed().as_secs_f64() * 1000.0;
    append_perf_event(json!({
        "ts": now_iso(),
        "action": "start",
        "session_id": session_id,
        "total_ms": round3(start_total_ms),
        "phases": {
            "detect_screenshot_dir_ms": round3(screenshot_dir_ms),
            "resolve_audio_device_ms": round3(audio_device_ms),
            "spawn_recorder_ms": round3(spawn_recorder_ms),
            "parakeet_prewarm_ms": round3(prewarm_ms)
        },
        "audio_device_retry": audio_device_retry
    }));

    play_event_sound("start", cli);

    print_out(
        cli,
        format!(
            "Started session {}\nsession_dir: {}\naudio_path: {}\nscreenshot_source_dir: {}\naudio_device: {}\naudio_device_retry: {}\nstartup_ms: {}",
            session_id,
            session_dir.display(),
            audio_path.display(),
            screenshot_dir.display(),
            resolved_audio_device,
            audio_device_retry,
            round3(start_total_ms),
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
            "startup_ms": round3(start_total_ms),
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

pub(crate) fn cmd_stop(cli: &Cli, args: &StopArgs) -> Result<i32, AppError> {
    ensure_dirs()?;
    let perf_total = Instant::now();
    let state = load_active_state()?;

    let session_dir = PathBuf::from(&state.session_dir);
    let screenshots_dir = PathBuf::from(&state.screenshots_dir);
    let events_path = PathBuf::from(&state.events_path);

    let ended_epoch = unix_now();
    let ended_iso = now_iso();

    let mut stop_recorder_ms = 0.0;
    let mut write_ms = 0.0;
    let mut web_server_ms = 0.0;

    if !cli.dry_run {
        if let Some(pid) = state.clipboard_watcher_pid {
            stop_clipboard_watcher(pid, cli);
        }
        append_jsonl(
            &events_path,
            &json!({
                "ts": ended_iso,
                "type": "session_stopping",
                "session_id": state.session_id,
            }),
        )?;
    }

    if !cli.dry_run {
        let t_stop_recorder = Instant::now();
        if let Some(pid) = state.ffmpeg_pid {
            stop_recorder(pid, cli)?;
            thread::sleep(Duration::from_millis(120));
        }
        stop_recorder_ms = t_stop_recorder.elapsed().as_secs_f64() * 1000.0;
    }

    let source_dir = PathBuf::from(&state.screenshot_source_dir);
    if !source_dir.is_dir() {
        return Err(app_error(
            7,
            format!(
                "Screenshot source directory missing: {}",
                source_dir.display()
            ),
        ));
    }

    let t_move_screens = Instant::now();
    let prior_events = read_jsonl_values(&events_path);
    let mut shots = load_shots_for_session(&session_dir, &prior_events);
    let clips = clipboard_from_events(&prior_events);
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
    let move_screenshots_ms = t_move_screens.elapsed().as_secs_f64() * 1000.0;

    let t_transcribe = Instant::now();
    let (transcript_raw, transcription_meta) = run_transcription(&state, &session_dir, args, cli);
    let transcribe_ms = t_transcribe.elapsed().as_secs_f64() * 1000.0;

    let audio_duration = get_audio_duration_sec(Path::new(&state.audio_path));

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
    let render_ms = t_render.elapsed().as_secs_f64() * 1000.0;

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

        fs::write(&note_path, format!("{}\n", note_md)).map_err(|e| {
            app_error(
                1,
                format!("Failed to write note {}: {e}", note_path.display()),
            )
        })?;

        fs::write(&html_path, html_body).map_err(|e| {
            app_error(
                1,
                format!("Failed to write HTML note {}: {e}", html_path.display()),
            )
        })?;

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

        let _ = generate_sessions_index_html()?;

        clear_active_state()?;

        write_ms = t_write.elapsed().as_secs_f64() * 1000.0;

        let t_web = Instant::now();
        let _ = ensure_web_server(cli, false);
        web_server_ms = t_web.elapsed().as_secs_f64() * 1000.0;
    }

    let stop_total_ms = perf_total.elapsed().as_secs_f64() * 1000.0;
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
            "stop_recorder_ms": round3(stop_recorder_ms),
            "move_screenshots_ms": round3(move_screenshots_ms),
            "transcribe_ms": round3(transcribe_ms),
            "render_ms": round3(render_ms),
            "write_ms": round3(write_ms),
            "web_server_ms": round3(web_server_ms)
        },
        "transcription_perf": transcription_perf,
        "transcription_method": transcription_meta.get("method").and_then(|v| v.as_str()),
        "transcription_status": transcription_meta.get("status").and_then(|v| v.as_str())
    }));

    if !cli.dry_run {
        play_event_sound("stop", cli);
    }

    print_out(
        cli,
        format!(
            "Stopped session {}\nsession_dir: {}\nscreenshots_moved: {}\nscreenshots_total: {}\nclipboard_captures: {}\nnote: {}\nhtml: {}\nstop_ms: {}",
            state.session_id,
            session_dir.display(),
            moved_count,
            shots.len(),
            clips.len(),
            note_path.display(),
            html_path.display(),
            round3(stop_total_ms),
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
                "stop_recorder_ms": round3(stop_recorder_ms),
                "move_screenshots_ms": round3(move_screenshots_ms),
                "transcribe_ms": round3(transcribe_ms),
                "render_ms": round3(render_ms),
                "write_ms": round3(write_ms),
                "web_server_ms": round3(web_server_ms)
            },
            "transcription": transcription_meta,
            "dry_run": cli.dry_run,
        }),
    );

    Ok(0)
}
