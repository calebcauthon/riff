use chrono::{SecondsFormat, Utc};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod cli;
mod error;
mod history;
mod models;
mod paths;
mod reporting;
mod screenshots;
mod session_commands;
mod shot_modules;
mod transcription;

use crate::cli::{
    Cli, Commands, HtmlArgs, LiveArgs, ScreenshotUseArgs, StartArgs, StopArgs, ToggleArgs,
    WatchClipboardArgs,
};
use crate::error::{app_error, AppError};
use crate::history::{
    cmd_copy, cmd_list, cmd_perf, cmd_send, cmd_send_images, cmd_show, resolve_recent_session_dir,
    resolve_session_dir_by_id,
};
use crate::models::SessionState;
use crate::paths::{
    active_state_file, audio_device_cache_file, ensure_dirs, parakeet_server_pid_file,
    perf_log_file, watcher_python_cache_file, web_server_pid_file,
};
use crate::reporting::{generate_html_for_session, generate_sessions_index_html};
use crate::session_commands::{cmd_chunk, cmd_pause, cmd_shot, cmd_start, cmd_stop, cmd_unpause};
use crate::transcription::{
    default_parakeet_script, default_sound_picker_script, ensure_web_server,
    resolve_parakeet_model, resolve_parakeet_script, resolve_python_bin, touch_web_server,
    web_server_base_url,
};

pub(crate) const SUPPORTED_IMAGE_EXTS: &[&str] =
    &["png", "jpg", "jpeg", "webp", "tif", "tiff", "heic", "heif"];
pub(crate) const RIFF_VERSION: &str = env!("RIFF_VERSION");
pub(crate) const RIFF_BUILD_ID: &str = env!("RIFF_BUILD_ID");
pub(crate) const RIFF_LONG_VERSION: &str =
    concat!(env!("RIFF_VERSION"), "\nbuild: ", env!("RIFF_BUILD_ID"));

fn build_id() -> &'static str {
    RIFF_BUILD_ID
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn session_stamp() -> String {
    Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

pub(crate) fn print_out(cli: &Cli, message: impl AsRef<str>) {
    if !cli.quiet {
        println!("{}", message.as_ref());
    }
}

pub(crate) fn print_verbose(cli: &Cli, message: impl AsRef<str>) {
    if cli.verbose && !cli.quiet {
        eprintln!("[verbose] {}", message.as_ref());
    }
}

pub(crate) fn emit_json(cli: &Cli, payload: &Value) {
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(payload).unwrap_or_else(|_| "{}".to_string())
        );
    }
}

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn expand_env_refs(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'}' {
                j += 1;
            }
            if j < bytes.len() {
                let key = &value[(i + 2)..j];
                if !key.is_empty() && is_valid_env_key(key) {
                    if let Ok(v) = env::var(key) {
                        out.push_str(&v);
                    }
                    i = j + 1;
                    continue;
                }
            }
        } else if i + 1 < bytes.len() {
            let c = bytes[i + 1] as char;
            if c == '_' || c.is_ascii_alphabetic() {
                let mut j = i + 2;
                while j < bytes.len() {
                    let cj = bytes[j] as char;
                    if cj == '_' || cj.is_ascii_alphanumeric() {
                        j += 1;
                    } else {
                        break;
                    }
                }
                let key = &value[(i + 1)..j];
                if let Ok(v) = env::var(key) {
                    out.push_str(&v);
                }
                i = j;
                continue;
            }
        }

        out.push('$');
        i += 1;
    }
    out
}

fn parse_riffrc_assignment(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    let body = if let Some(rest) = trimmed.strip_prefix("export ") {
        rest.trim_start()
    } else {
        trimmed
    };
    let (key_raw, value_raw) = body.split_once('=')?;
    let key = key_raw.trim();
    if !is_valid_env_key(key) {
        return None;
    }

    let value = value_raw.trim();
    let unquoted = if value.len() >= 2 {
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            &value[1..value.len() - 1]
        } else {
            value
        }
    } else {
        value
    };

    Some((key.to_string(), expand_env_refs(unquoted)))
}

fn riffrc_path() -> Option<PathBuf> {
    if let Some(custom) = env::var_os("RIFF_RC_FILE") {
        return Some(PathBuf::from(custom));
    }
    env::var_os("HOME").map(|h| PathBuf::from(h).join(".riffrc"))
}

fn riff_json_config_path() -> Option<PathBuf> {
    if let Some(custom) = env::var_os("RIFF_CONFIG_JSON_FILE") {
        return Some(PathBuf::from(custom));
    }
    env::var_os("HOME").map(|h| PathBuf::from(h).join(".riff.json"))
}

fn maybe_set_default_env(
    original_env_keys: &HashSet<OsString>,
    key: &str,
    value: String,
    override_loaded_default: bool,
) {
    let key_os = OsString::from(key);
    if original_env_keys.contains(&key_os) {
        return;
    }
    if override_loaded_default || env::var_os(&key_os).is_none() {
        env::set_var(key, value);
    }
}

fn load_riffrc_defaults(original_env_keys: &HashSet<OsString>) {
    let Some(path) = riffrc_path() else {
        return;
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    for line in raw.lines() {
        if let Some((key, value)) = parse_riffrc_assignment(line) {
            if !key.starts_with("RIFF_") {
                continue;
            }
            maybe_set_default_env(original_env_keys, &key, value, false);
        }
    }
}

fn json_config_value_to_env_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(expand_env_refs(s)),
        Value::Bool(v) => Some(if *v { "1" } else { "0" }.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn load_riff_json_defaults(original_env_keys: &HashSet<OsString>) {
    let Some(path) = riff_json_config_path() else {
        return;
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    let Some(obj) = parsed.as_object() else {
        return;
    };

    for (key, value) in obj {
        if key.starts_with("RIFF_") {
            if let Some(rendered) = json_config_value_to_env_string(value) {
                maybe_set_default_env(original_env_keys, key, rendered, true);
            }
        }
    }

    if let Some(riff_obj) = obj.get("riff").and_then(|v| v.as_object()) {
        if let Some(post_cmd) = riff_obj.get("post_transcribe_cmd") {
            if let Some(rendered) = json_config_value_to_env_string(post_cmd) {
                maybe_set_default_env(
                    original_env_keys,
                    "RIFF_POST_TRANSCRIBE_CMD",
                    rendered,
                    true,
                );
            }
        }
        if let Some(transcribe_cmd) = riff_obj.get("transcribe_cmd") {
            if let Some(rendered) = json_config_value_to_env_string(transcribe_cmd) {
                maybe_set_default_env(original_env_keys, "RIFF_TRANSCRIBE_CMD", rendered, true);
            }
        }
    }
}

fn upsert_riffrc_export(key: &str, value: &str) -> Result<PathBuf, AppError> {
    let Some(path) = riffrc_path() else {
        return Err(app_error(
            1,
            "Cannot resolve rc path. Set HOME or RIFF_RC_FILE.",
        ));
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| app_error(1, format!("Failed to create {}: {e}", parent.display())))?;
    }

    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut replaced = false;
    for line in &mut lines {
        if let Some((parsed_key, _)) = parse_riffrc_assignment(line) {
            if parsed_key == key {
                *line = format!("export {key}={value}");
                replaced = true;
                break;
            }
        }
    }
    if !replaced {
        lines.push(format!("export {key}={value}"));
    }

    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    fs::write(&path, out)
        .map_err(|e| app_error(1, format!("Failed to write {}: {e}", path.display())))?;
    Ok(path)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, AppError> {
    let bytes = fs::read(path)
        .map_err(|e| app_error(1, format!("Failed to read {}: {e}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| app_error(1, format!("Failed to parse JSON {}: {e}", path.display())))
}

fn write_json<T: Serialize>(path: &Path, payload: &T) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| app_error(1, format!("Failed to create {}: {e}", parent.display())))?;
    }
    let text = serde_json::to_string_pretty(payload).map_err(|e| {
        app_error(
            1,
            format!("Failed to serialize JSON {}: {e}", path.display()),
        )
    })?;
    fs::write(path, text)
        .map_err(|e| app_error(1, format!("Failed to write {}: {e}", path.display())))
}

fn append_jsonl(path: &Path, payload: &Value) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| app_error(1, format!("Failed to create {}: {e}", parent.display())))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| app_error(1, format!("Failed to open {}: {e}", path.display())))?;
    let mut line = serde_json::to_string(payload)
        .map_err(|e| app_error(1, format!("Failed to serialize JSONL event: {e}")))?;
    line.push('\n');
    f.write_all(line.as_bytes())
        .map_err(|e| app_error(1, format!("Failed to append {}: {e}", path.display())))
}

fn append_perf_event(payload: Value) {
    if let Err(e) = append_jsonl(&perf_log_file(), &payload) {
        eprintln!("[perf] failed to append perf log: {}", e);
    }
}

fn bool_env_enabled(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => default,
    }
}

fn clipboard_monitor_enabled() -> bool {
    bool_env_enabled("RIFF_CLIPBOARD_MONITOR", true)
}

fn normalize_clipboard_text(raw: &str) -> String {
    raw.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

fn read_clipboard_text() -> Option<String> {
    if !command_exists("pbpaste") {
        return None;
    }
    let out = Command::new("pbpaste").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

fn monitor_clipboard_loop(args: &WatchClipboardArgs) -> Result<(), AppError> {
    if !clipboard_monitor_enabled() || !command_exists("pbpaste") {
        return Ok(());
    }

    let events_path = args.events_path.clone();
    let mut last_seen = read_clipboard_text()
        .map(|s| normalize_clipboard_text(&s))
        .unwrap_or_default();
    let mut next_id = args.start_id.saturating_add(1);

    loop {
        let Some(current_raw) = read_clipboard_text() else {
            thread::sleep(Duration::from_millis(args.poll_ms.max(100)));
            continue;
        };
        let current = normalize_clipboard_text(&current_raw);

        if !current.is_empty() && current != last_seen {
            let audio_sec = (unix_now() - args.started_at_epoch).max(0.0);
            let payload = json!({
                "ts": now_iso(),
                "type": "clipboard_copied",
                "id": next_id,
                "text": current,
                "audioSec": round3(audio_sec),
            });
            append_jsonl(&events_path, &payload)?;
            last_seen = payload
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            next_id = next_id.saturating_add(1);
        }

        thread::sleep(Duration::from_millis(args.poll_ms.max(100)));
    }
}

fn cmd_watch_clipboard(_cli: &Cli, args: &WatchClipboardArgs) -> Result<i32, AppError> {
    monitor_clipboard_loop(args)?;
    Ok(0)
}

pub(crate) fn spawn_clipboard_watcher(
    state: &SessionState,
    start_id: usize,
    cli: &Cli,
) -> Option<i32> {
    if !clipboard_monitor_enabled() {
        print_verbose(cli, "Clipboard watcher disabled by RIFF_CLIPBOARD_MONITOR.");
        return None;
    }
    if !command_exists("pbpaste") {
        print_verbose(
            cli,
            "Clipboard watcher not started because pbpaste is unavailable.",
        );
        return None;
    }

    let exe = env::current_exe().ok()?;
    let child = Command::new(exe)
        .arg("--quiet")
        .arg("watch-clipboard")
        .arg("--session-id")
        .arg(&state.session_id)
        .arg("--events-path")
        .arg(&state.events_path)
        .arg("--started-at-epoch")
        .arg(state.started_at_epoch.to_string())
        .arg("--start-id")
        .arg(start_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let pid = child.id() as i32;
    print_verbose(cli, format!("Clipboard watcher started with pid={pid}"));
    Some(pid)
}

pub(crate) fn stop_clipboard_watcher(pid: i32, cli: &Cli) {
    if !process_is_alive(pid) {
        return;
    }
    let _ = send_signal(pid, libc::SIGTERM);
    print_verbose(cli, format!("Clipboard watcher pid={pid} sent SIGTERM."));
}

fn transcription_worker_enabled() -> bool {
    bool_env_enabled("RIFF_LIVE_TRANSCRIBE", false)
}

fn env_f64(name: &str, default: f64, min: f64, max: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.clamp(min, max))
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v.clamp(min, max))
        .unwrap_or(default)
}

fn env_optional_chunk_max_sec() -> Option<f64> {
    let raw = env::var("RIFF_CHUNK_MAX_SEC")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    if raw <= 0.0 {
        None
    } else {
        Some(raw.clamp(5.0, 300.0))
    }
}

fn python_major_minor(bin: &str) -> Option<(u32, u32)> {
    let out = Command::new(bin)
        .arg("-c")
        .arg("import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (major, minor) = raw.split_once('.')?;
    Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?))
}

fn python_supported_for_parakeet(bin: &str) -> bool {
    matches!(
        python_major_minor(bin),
        Some((3, 10)) | Some((3, 11)) | Some((3, 12))
    )
}

fn python_has_parakeet_deps(bin: &str) -> bool {
    let out = Command::new(bin)
        .arg("-c")
        .arg("import torch; import nemo.collections.asr.models")
        .output();
    matches!(out, Ok(o) if o.status.success())
}

fn resolve_watcher_python_bin() -> (Option<String>, Vec<String>, bool) {
    let cache_file = watcher_python_cache_file();
    if let Ok(cached) = fs::read_to_string(&cache_file) {
        let cached = cached.trim();
        if !cached.is_empty() && command_exists(cached) && python_supported_for_parakeet(cached) {
            return (
                Some(cached.to_string()),
                vec![format!("{cached} (cached)")],
                true,
            );
        }
    }

    let mut candidates = Vec::<String>::new();
    let primary = resolve_python_bin(None);
    candidates.push(primary);
    for alt in ["python3.12", "python3.11", "python3.10"] {
        if !candidates.iter().any(|c| c == alt) {
            candidates.push(alt.to_string());
        }
    }

    let mut considered = Vec::<String>::new();
    for candidate in &candidates {
        if !command_exists(candidate) {
            continue;
        }
        let supported = python_supported_for_parakeet(candidate);
        let deps_ok = if supported {
            python_has_parakeet_deps(candidate)
        } else {
            false
        };
        let version = python_major_minor(candidate)
            .map(|(maj, min)| format!("{maj}.{min}"))
            .unwrap_or_else(|| "unknown".to_string());
        considered.push(format!(
            "{}@{}{}{}",
            candidate,
            version,
            if supported { "" } else { " (unsupported)" },
            if supported && !deps_ok {
                " (deps-missing)"
            } else {
                ""
            }
        ));
        if supported && deps_ok {
            let _ = fs::write(&cache_file, format!("{candidate}\n"));
            return (Some(candidate.clone()), considered, false);
        }
    }
    (None, considered, false)
}

fn append_transcription_watcher_event(state: &SessionState, payload: Value) {
    let mut event = json!({
        "ts": now_iso(),
        "session_id": state.session_id,
    });
    if let (Some(base), Some(extra)) = (event.as_object_mut(), payload.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    let _ = append_jsonl(Path::new(&state.events_path), &event);
}

pub(crate) fn spawn_transcription_watcher(state: &SessionState, cli: &Cli) -> Option<i32> {
    if !transcription_worker_enabled() {
        print_verbose(
            cli,
            "Transcription watcher disabled by RIFF_LIVE_TRANSCRIBE.",
        );
        append_transcription_watcher_event(
            state,
            json!({
                "type": "transcription_watcher_not_started",
                "reason": "disabled_by_env",
            }),
        );
        return None;
    }
    if env::var("RIFF_TRANSCRIBE_CMD")
        .ok()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        print_verbose(
            cli,
            "Transcription watcher not started because RIFF_TRANSCRIBE_CMD is set.",
        );
        append_transcription_watcher_event(
            state,
            json!({
                "type": "transcription_watcher_not_started",
                "reason": "custom_transcribe_cmd_enabled",
            }),
        );
        return None;
    }

    let (python_bin, python_candidates, python_from_cache) = resolve_watcher_python_bin();
    let Some(python_bin) = python_bin else {
        print_verbose(
            cli,
            "Transcription watcher not started because no supported python (3.10-3.12) was found.",
        );
        append_transcription_watcher_event(
            state,
            json!({
                "type": "transcription_watcher_not_started",
                "reason": "python_incompatible_or_unavailable_or_missing_parakeet_deps",
                "python_candidates": python_candidates,
            }),
        );
        return None;
    };

    let local_script = default_parakeet_script();
    let resolved_script = resolve_parakeet_script(None);
    let local_script_for_event = local_script.clone();
    let resolved_script_for_event = resolved_script.clone();
    let mut candidates = Vec::<PathBuf>::new();
    if let Some(path) = local_script {
        candidates.push(path);
    }
    if let Some(path) = resolved_script {
        if !candidates.iter().any(|p| p == &path) {
            candidates.push(path);
        }
    }
    let supports_watch_mode = |path: &Path| -> bool {
        fs::read_to_string(path)
            .map(|src| src.contains("--watch-audio"))
            .unwrap_or(false)
    };
    let script_path = match candidates
        .into_iter()
        .find(|path| path.exists() && supports_watch_mode(path))
    {
        Some(path) => path,
        None => {
            print_verbose(
                cli,
                "Transcription watcher not started because no watch-capable parakeet script was found.",
            );
            append_transcription_watcher_event(
                state,
                json!({
                    "type": "transcription_watcher_not_started",
                    "reason": "no_watch_capable_script",
                    "local_script": local_script_for_event.map(|p| p.display().to_string()),
                    "resolved_script": resolved_script_for_event.map(|p| p.display().to_string()),
                }),
            );
            return None;
        }
    };

    let session_dir = PathBuf::from(&state.session_dir);
    let log_path = session_dir.join("transcription-watcher.log");
    let log_file = match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => f,
        Err(e) => {
            append_transcription_watcher_event(
                state,
                json!({
                    "type": "transcription_watcher_not_started",
                    "reason": "open_log_failed",
                    "log_path": log_path,
                    "error": e.to_string(),
                }),
            );
            return None;
        }
    };
    let log_file_err = match log_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            append_transcription_watcher_event(
                state,
                json!({
                    "type": "transcription_watcher_not_started",
                    "reason": "clone_log_handle_failed",
                    "log_path": log_path,
                    "error": e.to_string(),
                }),
            );
            return None;
        }
    };

    let min_chunk_sec = env_f64("RIFF_CHUNK_MIN_SEC", 12.0, 3.0, 120.0);
    let max_chunk_sec = env_optional_chunk_max_sec();
    let silence_sec = env_f64("RIFF_CHUNK_SILENCE_SEC", 1.2, 0.2, 10.0);
    let silence_db = env_f64("RIFF_CHUNK_SILENCE_DB", -33.0, -90.0, -5.0);
    let poll_ms = env_u64("RIFF_CHUNK_POLL_MS", 800, 200, 5000);
    let model = resolve_parakeet_model(None);

    let watcher_args = vec![
        script_path.display().to_string(),
        "--watch-audio".to_string(),
        "--audio".to_string(),
        state.audio_path.clone(),
        "--out-txt".to_string(),
        session_dir.join("transcript.txt").display().to_string(),
        "--events-path".to_string(),
        state.events_path.clone(),
        "--session-id".to_string(),
        state.session_id.clone(),
        "--started-at-epoch".to_string(),
        state.started_at_epoch.to_string(),
        "--model".to_string(),
        model.clone(),
        "--min-chunk-sec".to_string(),
        format!("{min_chunk_sec:.3}"),
        "--max-chunk-sec".to_string(),
        format!("{:.3}", max_chunk_sec.unwrap_or(0.0)),
        "--silence-sec".to_string(),
        format!("{silence_sec:.3}"),
        "--silence-db".to_string(),
        format!("{silence_db:.3}"),
        "--poll-ms".to_string(),
        poll_ms.to_string(),
        "--quiet".to_string(),
    ];
    let command_preview = format!(
        "{} {}",
        shell_escape(&python_bin),
        watcher_args
            .iter()
            .map(|a| shell_escape(a))
            .collect::<Vec<_>>()
            .join(" ")
    );

    let mut cmd = Command::new(&python_bin);
    cmd.args(&watcher_args[0..])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err));

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            append_transcription_watcher_event(
                state,
                json!({
                    "type": "transcription_watcher_not_started",
                    "reason": "spawn_failed",
                    "error": e.to_string(),
                    "python_bin": python_bin,
                    "python_candidates": python_candidates,
                    "script_path": script_path,
                    "log_path": log_path,
                    "command_preview": command_preview,
                }),
            );
            return None;
        }
    };
    thread::sleep(Duration::from_millis(40));
    if child.try_wait().ok().flatten().is_some() {
        print_verbose(
            cli,
            format!(
                "Transcription watcher exited immediately; see {}",
                log_path.display()
            ),
        );
        append_transcription_watcher_event(
            state,
            json!({
                "type": "transcription_watcher_exited_early",
                "reason": "exited_within_startup_window",
                "python_bin": python_bin,
                "python_from_cache": python_from_cache,
                "python_candidates": python_candidates,
                "script_path": script_path,
                "log_path": log_path,
                "command_preview": command_preview,
            }),
        );
        if python_from_cache {
            let _ = fs::remove_file(watcher_python_cache_file());
        }
        return None;
    }

    let pid = child.id() as i32;
    append_transcription_watcher_event(
        state,
        json!({
            "type": "transcription_watcher_started",
            "pid": pid,
            "python_bin": python_bin,
            "python_from_cache": python_from_cache,
            "python_candidates": python_candidates,
            "script_path": script_path,
            "log_path": log_path,
            "command_preview": command_preview,
        }),
    );
    print_verbose(
        cli,
        format!(
            "Transcription watcher started with pid={pid}, log={}",
            log_path.display()
        ),
    );
    Some(pid)
}

pub(crate) fn wait_for_transcription_watcher(
    pid: i32,
    timeout: Duration,
    cli: &Cli,
) -> (bool, f64) {
    let started = SystemTime::now();
    while SystemTime::now()
        .duration_since(started)
        .unwrap_or_else(|_| Duration::from_secs(0))
        < timeout
    {
        if !process_is_alive(pid) {
            let waited_ms = SystemTime::now()
                .duration_since(started)
                .unwrap_or_else(|_| Duration::from_secs(0))
                .as_secs_f64()
                * 1000.0;
            return (true, waited_ms);
        }
        thread::sleep(Duration::from_millis(80));
    }
    let waited_ms = SystemTime::now()
        .duration_since(started)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs_f64()
        * 1000.0;
    print_verbose(
        cli,
        format!("Transcription watcher pid={pid} still alive after wait timeout."),
    );
    (false, waited_ms)
}

pub(crate) fn stop_transcription_watcher(pid: i32, cli: &Cli) {
    if !process_is_alive(pid) {
        return;
    }
    let _ = send_signal(pid, libc::SIGTERM);
    let deadline = SystemTime::now() + Duration::from_millis(1200);
    while SystemTime::now() < deadline {
        if !process_is_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(40));
    }
    let _ = send_signal(pid, libc::SIGKILL);
    print_verbose(
        cli,
        format!("Transcription watcher pid={pid} was force-stopped."),
    );
}

fn resolve_sound_path(spec: &str) -> PathBuf {
    if spec.contains('/') {
        return PathBuf::from(spec);
    }

    let mut name = spec.to_string();
    if !name.ends_with(".aiff") {
        name.push_str(".aiff");
    }

    PathBuf::from("/System/Library/Sounds").join(name)
}

fn env_beep_count(kind: &str) -> u8 {
    let key = if kind == "start" {
        "RIFF_BEEP_START_COUNT"
    } else {
        "RIFF_BEEP_STOP_COUNT"
    };

    let parsed = env::var(key)
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(1);

    parsed.clamp(1, 3)
}

fn env_beep_gap_sec() -> f32 {
    let parsed = env::var("RIFF_BEEP_GAP_SEC")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(0.08);

    parsed.clamp(0.0, 1.0)
}

fn play_event_sound(kind: &str, cli: &Cli) {
    if cli.no_beeps {
        return;
    }
    if !bool_env_enabled("RIFF_BEEP", true) {
        return;
    }

    let env_key = if kind == "start" {
        "RIFF_BEEP_START"
    } else {
        "RIFF_BEEP_STOP"
    };
    let default_sound = if kind == "start" { "Ping" } else { "Glass" };
    let sound_spec = env::var(env_key).unwrap_or_else(|_| default_sound.to_string());
    let sound_path = resolve_sound_path(&sound_spec);
    let count = env_beep_count(kind);
    let gap_sec = env_beep_gap_sec();

    if command_exists("afplay") && sound_path.exists() {
        // Spawn detached shell loop so beeps can continue even after this process exits.
        let _ = Command::new("sh")
            .arg("-c")
            .arg(
                "count=\"$1\"; path=\"$2\"; gap=\"$3\"; i=1; pids=\"\"; while [ \"$i\" -le \"$count\" ]; do afplay \"$path\" >/dev/null 2>&1 & p=\"$!\"; pids=\"$pids $p\"; i=$((i+1)); [ \"$i\" -le \"$count\" ] && sleep \"$gap\"; done; for p in $pids; do wait \"$p\" 2>/dev/null || true; done",
            )
            .arg("riff-beep")
            .arg(count.to_string())
            .arg(sound_path.as_os_str())
            .arg(format!("{:.2}", gap_sec))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        return;
    }

    if command_exists("osascript") {
        let script = format!("beep {}", count);
        let _ = Command::new("osascript")
            .args(["-e", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if cli.verbose && !cli.quiet {
            eprintln!("[verbose] fallback beep used for {} x{}", kind, count);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FrontmostAppMeta {
    pub name: String,
    pub bundle_id: Option<String>,
    pub pid: Option<i32>,
    pub window_title: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProcessStats {
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub rss_kb: u64,
    pub elapsed: Option<String>,
    pub state: Option<String>,
    pub command: Option<String>,
}

fn parse_optional_field(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn capture_process_stats(pid: i32, cli: &Cli) -> Result<ProcessStats, String> {
    if !command_exists("ps") {
        return Err("ps_unavailable".to_string());
    }

    let out = match Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .args([
            "-o", "%cpu=", "-o", "%mem=", "-o", "rss=", "-o", "etime=", "-o", "state=", "-o",
            "command=",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let msg = format!("ps_failed_status_{:?}", o.status.code());
            if cli.verbose && !cli.quiet {
                eprintln!("[verbose] could not capture process stats ({msg})");
            }
            return Err(msg);
        }
        Err(e) => {
            let msg = format!("ps_exec_error_{e}");
            if cli.verbose && !cli.quiet {
                eprintln!("[verbose] could not run ps for process stats: {msg}");
            }
            return Err(msg);
        }
    };

    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| "ps_empty_output".to_string())?;

    let cols = line.split_whitespace().collect::<Vec<_>>();
    if cols.len() < 6 {
        return Err("ps_unexpected_output".to_string());
    }

    let cpu_percent = cols[0]
        .trim()
        .parse::<f64>()
        .map_err(|_| "ps_parse_cpu_failed".to_string())?;
    let mem_percent = cols[1]
        .trim()
        .parse::<f64>()
        .map_err(|_| "ps_parse_mem_failed".to_string())?;
    let rss_kb = cols[2]
        .trim()
        .parse::<u64>()
        .map_err(|_| "ps_parse_rss_failed".to_string())?;
    let elapsed = parse_optional_field(cols[3]);
    let state = parse_optional_field(cols[4]);
    let command = parse_optional_field(&cols[5..].join(" "));

    Ok(ProcessStats {
        cpu_percent,
        mem_percent,
        rss_kb,
        elapsed,
        state,
        command,
    })
}

pub(crate) fn capture_frontmost_app_meta(cli: &Cli) -> Result<FrontmostAppMeta, String> {
    if !cfg!(target_os = "macos") || !command_exists("osascript") {
        return Err("osascript_unavailable".to_string());
    }

    let script = r#"
set appName to ""
set bundleID to ""
set pidValue to ""
set winTitle to ""
tell application "System Events"
  set frontApp to first application process whose frontmost is true
  set appName to name of frontApp
  try
    set bundleID to bundle identifier of frontApp
  end try
  try
    set pidValue to (unix id of frontApp) as string
  end try
  try
    set winTitle to name of front window of frontApp
  end try
end tell
return appName & tab & bundleID & tab & pidValue & tab & winTitle
"#;

    let out = match Command::new("osascript").args(["-e", script]).output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let msg = format!("osascript_failed_status_{:?}", o.status.code());
            if cli.verbose && !cli.quiet {
                eprintln!("[verbose] could not capture frontmost app metadata ({msg})");
            }
            return Err(msg);
        }
        Err(e) => {
            let msg = format!("osascript_exec_error_{e}");
            if cli.verbose && !cli.quiet {
                eprintln!("[verbose] could not run osascript for app metadata: {msg}");
            }
            return Err(msg);
        }
    };

    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if line.is_empty() {
        return Err("empty_osascript_output".to_string());
    }

    let mut parts = line.splitn(4, '\t');
    let app_name = parts.next().unwrap_or("").trim().to_string();
    if app_name.is_empty() {
        return Err("missing_frontmost_app_name".to_string());
    }

    let bundle_id = parse_optional_field(parts.next().unwrap_or(""));
    let pid =
        parse_optional_field(parts.next().unwrap_or("")).and_then(|raw| raw.parse::<i32>().ok());
    let window_title = parse_optional_field(parts.next().unwrap_or(""));

    Ok(FrontmostAppMeta {
        name: app_name,
        bundle_id,
        pid,
        window_title,
    })
}

pub(crate) fn command_exists(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).exists();
    }
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|p| p.join(cmd).exists()))
        .unwrap_or(false)
}

fn parse_avfoundation_audio_devices(raw: &str) -> Vec<(String, String)> {
    let mut devices = Vec::new();
    let mut in_audio_section = false;

    for line in raw.lines() {
        let l = line.trim();
        if l.contains("AVFoundation audio devices") {
            in_audio_section = true;
            continue;
        }
        if l.contains("AVFoundation video devices") {
            in_audio_section = false;
            continue;
        }
        if !in_audio_section {
            continue;
        }

        let Some(open) = l.rfind('[') else {
            continue;
        };
        let rest = &l[open + 1..];
        let Some(close_rel) = rest.find(']') else {
            continue;
        };

        let idx = rest[..close_rel].trim();
        if idx.is_empty() || !idx.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let name = rest[close_rel + 1..].trim();
        if name.is_empty() {
            continue;
        }

        devices.push((idx.to_string(), name.to_string()));
    }

    devices
}

fn list_avfoundation_audio_devices() -> Vec<(String, String)> {
    if !command_exists("ffmpeg") {
        return Vec::new();
    }

    let out = match Command::new("ffmpeg")
        .args(["-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let merged = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    parse_avfoundation_audio_devices(&merged)
}

fn contains_any(name_lc: &str, terms: &[&str]) -> bool {
    terms.iter().any(|t| name_lc.contains(t))
}

fn resolve_audio_device_with_cache(requested: &str, cli: &Cli, use_cache: bool) -> String {
    if !requested.eq_ignore_ascii_case("auto") {
        return requested.to_string();
    }

    let cache_file = audio_device_cache_file();
    if use_cache {
        if let Ok(cached) = fs::read_to_string(&cache_file) {
            let cached = cached.trim();
            if cached.starts_with(':') && cached.len() > 1 {
                // Trust the cached device for the fast path. If it goes stale, recorder startup
                // already has a retry path that clears the cache and re-detects.
                print_verbose(cli, format!("Using cached audio device {}", cached));
                return cached.to_string();
            }
        }
    }

    let avoid = ["iphone", "continuity"];
    let built_in = ["macbook", "built-in", "internal"];
    let devices = list_avfoundation_audio_devices();

    if devices.is_empty() {
        print_verbose(
            cli,
            "Could not auto-detect macOS audio devices; falling back to :0",
        );
        return ":0".to_string();
    }

    let preferred = devices
        .iter()
        .find(|(_, name)| {
            let lc = name.to_ascii_lowercase();
            contains_any(&lc, &built_in) && !contains_any(&lc, &avoid)
        })
        .or_else(|| {
            devices.iter().find(|(_, name)| {
                let lc = name.to_ascii_lowercase();
                !contains_any(&lc, &avoid)
            })
        })
        .unwrap_or(&devices[0]);

    let resolved = format!(":{}", preferred.0);
    let _ = fs::write(&cache_file, format!("{}\n", resolved));
    print_verbose(
        cli,
        format!("Auto-selected audio device {} ({})", resolved, preferred.1),
    );
    resolved
}

fn resolve_audio_device(requested: &str, cli: &Cli) -> String {
    resolve_audio_device_with_cache(requested, cli, true)
}

fn resolve_audio_device_uncached(cli: &Cli) -> String {
    resolve_audio_device_with_cache("auto", cli, false)
}

fn recorder_error_looks_like_invalid_audio_device(err: &AppError) -> bool {
    let m = err.message.to_ascii_lowercase();
    m.contains("invalid audio device index")
        || m.contains("error opening input file")
        || m.contains("avfoundation indev") && m.contains("input/output error")
}

pub(crate) fn process_is_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    matches!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM))
}

fn send_signal(pid: i32, signal: i32) -> io::Result<()> {
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn read_pid_file(path: &Path) -> Option<i32> {
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse::<i32>().ok()
}

pub(crate) fn write_pid_file(path: &Path, pid: i32) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, format!("{}\n", pid));
}

fn load_active_state() -> Result<SessionState, AppError> {
    let path = active_state_file();
    if !path.exists() {
        return Err(app_error(4, "No active session. Run 'riff start' first."));
    }
    read_json(&path)
}

fn save_active_state(state: &SessionState) -> Result<(), AppError> {
    write_json(&active_state_file(), state)
}

fn clear_active_state() -> Result<(), AppError> {
    let path = active_state_file();
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|e| app_error(1, format!("Failed to remove {}: {e}", path.display())))?;
    }
    Ok(())
}

fn build_record_cmd(audio_path: &Path, audio_device: &str) -> Vec<String> {
    vec![
        "ffmpeg".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-f".to_string(),
        "avfoundation".to_string(),
        "-i".to_string(),
        audio_device.to_string(),
        "-ac".to_string(),
        "1".to_string(),
        "-ar".to_string(),
        "16000".to_string(),
        "-c:a".to_string(),
        "pcm_s16le".to_string(),
        audio_path.display().to_string(),
    ]
}

fn wait_for_process_start(child: &mut Child, timeout: Duration) -> Result<bool, AppError> {
    let started = SystemTime::now();
    loop {
        if let Some(_status) = child
            .try_wait()
            .map_err(|e| app_error(1, format!("Failed waiting for recorder process: {e}")))?
        {
            return Ok(false);
        }
        if SystemTime::now()
            .duration_since(started)
            .unwrap_or_else(|_| Duration::from_secs(0))
            > timeout
        {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_tail(path: &Path, max_bytes: usize) -> String {
    let mut buf = Vec::new();
    if let Ok(mut f) = File::open(path) {
        let _ = f.read_to_end(&mut buf);
    }
    if buf.len() > max_bytes {
        let slice = &buf[buf.len() - max_bytes..];
        String::from_utf8_lossy(slice).to_string()
    } else {
        String::from_utf8_lossy(&buf).to_string()
    }
}

fn spawn_recorder(
    record_cmd: &[String],
    ffmpeg_log_path: &Path,
    cli: &Cli,
) -> Result<i32, AppError> {
    if record_cmd.is_empty() {
        return Err(app_error(1, "Internal error: empty recorder command"));
    }

    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ffmpeg_log_path)
        .map_err(|e| {
            app_error(
                1,
                format!("Failed to open {}: {e}", ffmpeg_log_path.display()),
            )
        })?;

    let command_str = record_cmd
        .iter()
        .map(|x| shell_escape(x))
        .collect::<Vec<_>>()
        .join(" ");
    let _ = writeln!(log, "[{}] start_cmd={}", now_iso(), command_str);

    print_verbose(cli, format!("Starting recorder: {command_str}"));

    let log_clone = log
        .try_clone()
        .map_err(|e| app_error(1, format!("Failed to clone ffmpeg log file handle: {e}")))?;

    let mut child = Command::new(&record_cmd[0])
        .args(&record_cmd[1..])
        .stdout(Stdio::from(log_clone))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| app_error(6, format!("Failed to start ffmpeg recorder: {e}")))?;

    let started = wait_for_process_start(&mut child, Duration::from_millis(120))?;
    if !started {
        let tail = read_tail(ffmpeg_log_path, 1200);
        return Err(app_error(
            6,
            format!(
                "Audio recorder exited immediately. Check audio device / permissions.\nffmpeg log tail:\n{}",
                tail
            ),
        ));
    }

    let pid = child.id() as i32;
    print_verbose(cli, format!("Recorder started with pid={pid}"));
    Ok(pid)
}

fn stop_recorder(pid: i32, cli: &Cli) -> Result<(), AppError> {
    if !process_is_alive(pid) {
        print_verbose(
            cli,
            format!("Recorder pid={pid} was not alive at stop time."),
        );
        return Ok(());
    }

    print_verbose(cli, format!("Stopping recorder pid={pid} with SIGINT"));
    send_signal(pid, libc::SIGINT)
        .map_err(|e| app_error(1, format!("Failed to SIGINT recorder pid={pid}: {e}")))?;

    let deadline = SystemTime::now() + Duration::from_secs(8);
    while SystemTime::now() < deadline {
        if !process_is_alive(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    print_verbose(
        cli,
        format!("Recorder pid={pid} still alive; sending SIGTERM"),
    );
    send_signal(pid, libc::SIGTERM)
        .map_err(|e| app_error(1, format!("Failed to SIGTERM recorder pid={pid}: {e}")))?;

    Ok(())
}

pub(crate) fn pause_recorder_capture(pid: i32, cli: &Cli) -> Result<(), AppError> {
    if !process_is_alive(pid) {
        return Err(app_error(
            1,
            format!("Recorder pid={pid} is not alive; cannot pause."),
        ));
    }
    print_verbose(cli, format!("Pausing recorder pid={pid} with SIGSTOP"));
    send_signal(pid, libc::SIGSTOP)
        .map_err(|e| app_error(1, format!("Failed to SIGSTOP recorder pid={pid}: {e}")))
}

pub(crate) fn resume_recorder_capture(pid: i32, cli: &Cli) -> Result<(), AppError> {
    if !process_is_alive(pid) {
        return Err(app_error(
            1,
            format!("Recorder pid={pid} is not alive; cannot resume."),
        ));
    }
    print_verbose(cli, format!("Resuming recorder pid={pid} with SIGCONT"));
    send_signal(pid, libc::SIGCONT)
        .map_err(|e| app_error(1, format!("Failed to SIGCONT recorder pid={pid}: {e}")))
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

pub(crate) fn shell_escape(text: &str) -> String {
    if text.is_empty() {
        return "''".to_string();
    }
    let escaped = text.replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

pub(crate) fn fill_template(
    template: &str,
    audio: &Path,
    out_base: &Path,
    out_txt: &Path,
    session_dir: &Path,
) -> String {
    fill_template_with_transcript(template, audio, out_base, out_txt, session_dir, None)
}

pub(crate) fn fill_template_with_transcript(
    template: &str,
    audio: &Path,
    out_base: &Path,
    out_txt: &Path,
    session_dir: &Path,
    transcript: Option<&str>,
) -> String {
    let mut s = template.to_string();
    s = s.replace("{audio}", &shell_escape(&audio.display().to_string()));
    s = s.replace("{out_base}", &shell_escape(&out_base.display().to_string()));
    s = s.replace("{out_txt}", &shell_escape(&out_txt.display().to_string()));
    s = s.replace(
        "{session_dir}",
        &shell_escape(&session_dir.display().to_string()),
    );
    if let Some(text) = transcript {
        s = s.replace("{transcript}", &shell_escape(text));
    }
    s
}

pub(crate) fn get_audio_duration_sec(audio_path: &Path) -> Option<f64> {
    if let Some(duration) = wav_duration_sec(audio_path) {
        return Some(duration);
    }

    if !command_exists("ffprobe") {
        return None;
    }

    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(audio_path)
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
}

fn wav_duration_sec(audio_path: &Path) -> Option<f64> {
    let mut file = File::open(audio_path).ok()?;
    let mut header = [0u8; 44];
    file.read_exact(&mut header).ok()?;

    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return None;
    }

    let channels = u16::from_le_bytes([header[22], header[23]]) as u64;
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]) as u64;
    let bits_per_sample = u16::from_le_bytes([header[34], header[35]]) as u64;
    let data_size = u32::from_le_bytes([header[40], header[41], header[42], header[43]]) as u64;

    if channels == 0 || sample_rate == 0 || bits_per_sample == 0 {
        return None;
    }

    let bytes_per_sample = bits_per_sample.div_ceil(8);
    let bytes_per_second = channels * sample_rate * bytes_per_sample;
    if bytes_per_second == 0 {
        return None;
    }

    Some(data_size as f64 / bytes_per_second as f64)
}

fn cmd_sounds(_cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;

    let script_path = env::var("RIFF_SOUND_PICKER_SCRIPT")
        .ok()
        .map(PathBuf::from)
        .or_else(default_sound_picker_script)
        .ok_or_else(|| {
            app_error(
                1,
                "Could not find sound picker script. Expected scripts/pick_riff_sounds.sh",
            )
        })?;

    let status = Command::new("bash")
        .arg(&script_path)
        .status()
        .map_err(|e| app_error(1, format!("Failed to run sound picker: {e}")))?;

    if !status.success() {
        return Err(app_error(
            status.code().unwrap_or(1),
            format!("Sound picker exited with status: {status}"),
        ));
    }

    Ok(0)
}

fn set_global_beep_enabled(cli: &Cli, enabled: bool) -> Result<i32, AppError> {
    let value = if enabled { "1" } else { "0" };
    if cli.dry_run {
        let path = riffrc_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.riffrc".to_string());
        print_out(
            cli,
            format!("[dry-run] Would write export RIFF_BEEP={value} to {path}"),
        );
        emit_json(
            cli,
            &json!({
                "ok": true,
                "beeps_enabled": enabled,
                "riffrc": path,
                "dry_run": true
            }),
        );
        return Ok(0);
    }

    let path = upsert_riffrc_export("RIFF_BEEP", value)?;
    env::set_var("RIFF_BEEP", value);
    let action = if enabled { "loud" } else { "silence" };
    print_out(
        cli,
        format!(
            "Global beeps {}.\nrc_file: {}",
            if enabled { "enabled" } else { "disabled" },
            path.display()
        ),
    );
    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": action,
            "beeps_enabled": enabled,
            "riffrc": path,
            "dry_run": false
        }),
    );
    Ok(0)
}

fn cmd_silence(cli: &Cli) -> Result<i32, AppError> {
    set_global_beep_enabled(cli, false)
}

fn cmd_loud(cli: &Cli) -> Result<i32, AppError> {
    set_global_beep_enabled(cli, true)
}

fn latest_transcription_watcher_event(events_path: &Path) -> Option<Value> {
    let text = fs::read_to_string(events_path).ok()?;
    for line in text.lines().rev() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(t) else {
            continue;
        };
        let et = v.get("type").and_then(|x| x.as_str()).unwrap_or_default();
        if matches!(
            et,
            "transcription_watcher_started"
                | "transcription_watcher_not_started"
                | "transcription_watcher_exited_early"
                | "transcription_worker_stopped"
                | "transcript_probe"
                | "transcript_chunk"
        ) {
            return Some(v);
        }
    }
    None
}

fn cmd_status(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;
    let active = active_state_file();
    if !active.exists() {
        print_out(cli, format!("No active session.\nbuild_id: {}", build_id()));
        emit_json(
            cli,
            &json!({
                "active": false,
                "build_id": build_id()
            }),
        );
        return Ok(0);
    }

    let state: SessionState = read_json(&active)?;
    let pid = state.ffmpeg_pid;
    let alive = pid.map(process_is_alive).unwrap_or(false);
    let watcher_pid = state.clipboard_watcher_pid;
    let watcher_alive = watcher_pid.map(process_is_alive).unwrap_or(false);
    let transcription_watcher_pid = state.transcription_watcher_pid;
    let transcription_watcher_alive = transcription_watcher_pid
        .map(process_is_alive)
        .unwrap_or(false);
    let watcher_event = latest_transcription_watcher_event(Path::new(&state.events_path));
    let watcher_event_type = watcher_event
        .as_ref()
        .and_then(|v| v.get("type").and_then(|x| x.as_str()))
        .map(|s| s.to_string());
    let watcher_reason = watcher_event
        .as_ref()
        .and_then(|v| v.get("reason").and_then(|x| x.as_str()))
        .map(|s| s.to_string());
    let watcher_log_path = watcher_event
        .as_ref()
        .and_then(|v| v.get("log_path").and_then(|x| x.as_str()))
        .map(|s| s.to_string());
    let watcher_command_preview = watcher_event
        .as_ref()
        .and_then(|v| v.get("command_preview").and_then(|x| x.as_str()))
        .map(|s| s.to_string());
    let pause_since = state.transcription_pause_started_sec;
    let paused_for_sec = if state.transcription_paused {
        pause_since.map(|t| (unix_now() - t).max(0.0))
    } else {
        None
    };

    print_out(
        cli,
        format!(
            "Active session: {}\nsession_dir: {}\nffmpeg_pid: {} (alive={})\nclipboard_watcher_pid: {} (alive={})\ntranscription_watcher_pid: {} (alive={})\ntranscription_watcher_event: {}{}\ntranscription_watcher_log: {}\ntranscription_cursor_sec: {:.3}\ntranscription_paused: {}{}\nbuild_id: {}",
            state.session_id,
            state.session_dir,
            pid.map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string()),
            alive
            ,
            watcher_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string()),
            watcher_alive,
            transcription_watcher_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string()),
            transcription_watcher_alive,
            watcher_event_type.as_deref().unwrap_or("none"),
            watcher_reason
                .as_deref()
                .map(|r| format!(" (reason={r})"))
                .unwrap_or_default(),
            watcher_log_path.as_deref().unwrap_or("none"),
            state.transcription_cursor_sec,
            state.transcription_paused,
            paused_for_sec
                .map(|sec| format!(" (paused_for={}s)", round3(sec)))
                .unwrap_or_default(),
            build_id()
        ),
    );

    emit_json(
        cli,
        &json!({
            "active": true,
            "session_id": state.session_id,
            "session_dir": state.session_dir,
            "started_at_iso": state.started_at_iso,
            "ffmpeg_pid": pid,
            "ffmpeg_alive": alive,
            "clipboard_watcher_pid": watcher_pid,
            "clipboard_watcher_alive": watcher_alive,
            "transcription_watcher_pid": transcription_watcher_pid,
            "transcription_watcher_alive": transcription_watcher_alive,
            "transcription_watcher_last_event": watcher_event_type,
            "transcription_watcher_last_reason": watcher_reason,
            "transcription_watcher_log_path": watcher_log_path,
            "transcription_watcher_command_preview": watcher_command_preview,
            "transcription_cursor_sec": round3(state.transcription_cursor_sec),
            "transcription_paused": state.transcription_paused,
            "transcription_pause_started_sec": pause_since.map(round3),
            "transcription_paused_for_sec": paused_for_sec.map(round3),
            "build_id": build_id(),
        }),
    );

    Ok(0)
}

fn format_hms_compact(seconds: f64) -> String {
    let sec = seconds.max(0.0).round() as i64;
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    let s = sec % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[derive(Default, Debug, Clone, Copy)]
struct LiveChunkStats {
    total: usize,
    ok: usize,
    skipped: usize,
    error: usize,
}

fn live_chunk_stats(events: &[Value]) -> LiveChunkStats {
    let mut stats = LiveChunkStats::default();
    for e in events {
        if e.get("type").and_then(|v| v.as_str()) != Some("transcript_chunk") {
            continue;
        }
        stats.total = stats.total.saturating_add(1);
        match e.get("status").and_then(|v| v.as_str()) {
            Some("ok") => stats.ok = stats.ok.saturating_add(1),
            Some("skipped") => stats.skipped = stats.skipped.saturating_add(1),
            Some("error") => stats.error = stats.error.saturating_add(1),
            _ => {}
        }
    }
    stats
}

fn live_snapshot(state: &SessionState) -> (f64, usize, usize, LiveChunkStats, String) {
    let elapsed = (unix_now() - state.started_at_epoch).max(0.0);
    let events = history::read_jsonl_values(Path::new(&state.events_path));
    let screenshots = reporting::shots_from_events(&events).len();
    let chunks = live_chunk_stats(&events);
    let transcript_path = Path::new(&state.session_dir).join("transcript.txt");
    let transcript = fs::read_to_string(transcript_path).unwrap_or_default();
    let words = transcript.split_whitespace().count();
    (elapsed, screenshots, words, chunks, transcript)
}

fn cmd_live(cli: &Cli, args: &LiveArgs) -> Result<i32, AppError> {
    ensure_dirs()?;
    let active = active_state_file();
    if !active.exists() {
        print_out(cli, "No active session.");
        emit_json(
            cli,
            &json!({
                "active": false
            }),
        );
        return Ok(0);
    }

    let poll_ms = args.poll_ms.max(200);
    let mut printed_banner = false;
    let mut last_transcript = String::new();
    loop {
        if !active.exists() {
            if !args.once {
                println!();
                print_out(cli, "Session ended.");
            }
            break;
        }

        let state: SessionState = read_json(&active)?;
        let (elapsed, screenshots, words, chunks, transcript) = live_snapshot(&state);
        if !printed_banner && !args.once {
            print_out(
                cli,
                "Manual chunking enabled: run `riff chunk` to process captured audio.",
            );
            print_out(
                cli,
                "Use `riff pause` / `riff unpause` to skip transcription windows.",
            );
            print_out(cli, format!("Session: {}", state.session_id));
            printed_banner = true;
        }
        let listen_state = if state.transcription_paused {
            let paused_for = state
                .transcription_pause_started_sec
                .map(|t| (unix_now() - t).max(0.0))
                .unwrap_or(0.0);
            format!("Paused {}", format_hms_compact(paused_for))
        } else {
            "Listening".to_string()
        };
        let line = format!(
            "LIVE • {} • {} screenshots • {} words • chunks {} (ok {}, skipped {}, err {}) • {}",
            format_hms_compact(elapsed),
            screenshots,
            words,
            chunks.total,
            chunks.ok,
            chunks.skipped,
            chunks.error,
            listen_state
        );

        let transcript_trimmed = transcript.trim().to_string();
        let new_words = if transcript_trimmed.starts_with(&last_transcript) {
            transcript_trimmed[last_transcript.len()..]
                .trim()
                .to_string()
        } else if transcript_trimmed != last_transcript {
            transcript_trimmed.clone()
        } else {
            String::new()
        };

        if args.once {
            print_out(cli, "Manual chunking enabled.");
            print_out(cli, &line);
            if !transcript_trimmed.is_empty() {
                print_out(cli, format!("Transcript: {}", transcript_trimmed));
            }
            emit_json(
                cli,
                &json!({
                    "active": true,
                    "session_id": state.session_id,
                    "elapsed_sec": round3(elapsed),
                    "screenshots": screenshots,
                    "words": words,
                    "chunks_total": chunks.total,
                    "chunks_ok": chunks.ok,
                    "chunks_skipped": chunks.skipped,
                    "chunks_error": chunks.error,
                    "transcription_cursor_sec": round3(state.transcription_cursor_sec),
                    "transcription_paused": state.transcription_paused,
                    "transcription_pause_started_sec": state.transcription_pause_started_sec.map(round3),
                    "transcript_text": transcript_trimmed,
                }),
            );
            break;
        }

        if !cli.quiet {
            print!("\r{line}\x1b[K");
            let _ = io::stdout().flush();
            if !new_words.is_empty() {
                print!("\r\x1b[K\nTranscript: {}\n", new_words);
                let _ = io::stdout().flush();
            }
        }
        last_transcript = transcript_trimmed;
        thread::sleep(Duration::from_millis(poll_ms));
    }

    Ok(0)
}

fn cmd_html(cli: &Cli, args: &HtmlArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let session_dir = if let Some(session_id) = args.session_id.as_deref() {
        resolve_session_dir_by_id(session_id)?
    } else {
        resolve_recent_session_dir(1)?
    };

    // Always regenerate so HTML reflects latest template/features.
    let html_path = generate_html_for_session(&session_dir)?;
    let index_path = generate_sessions_index_html()?;

    let session_id = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    // Print filesystem path first so it is easy to capture in scripts.
    println!("{}", html_path.display());

    let base_url = web_server_base_url();
    let server_ready = ensure_web_server(cli, true);
    let mut opened_target = html_path.display().to_string();

    if server_ready {
        let _ = touch_web_server(&base_url); // reset idle timeout clock
        opened_target = format!(
            "{}/sessions/{}/note.html",
            base_url.trim_end_matches('/'),
            session_id
        );
    }

    if !cli.quiet {
        println!("Opening {}", opened_target);
    }

    let status = Command::new("open")
        .arg(OsString::from(&opened_target))
        .status()
        .map_err(|e| app_error(1, format!("Failed to run 'open': {e}")))?;
    if !status.success() {
        return Err(app_error(
            1,
            format!("open command failed with status: {status}"),
        ));
    }

    emit_json(
        cli,
        &json!({
            "ok": true,
            "session_dir": session_dir,
            "html_path": html_path,
            "sessions_index_path": index_path,
            "opened": true,
            "opened_target": opened_target,
            "web_server_ready": server_ready,
            "web_server_url": if server_ready { Value::String(base_url) } else { Value::Null },
        }),
    );

    Ok(0)
}

fn kill_server_from_pid_file(
    cli: &Cli,
    label: &str,
    pid_file: &Path,
    report: &mut Vec<Value>,
) -> Result<(), AppError> {
    let pid = read_pid_file(pid_file);
    if pid.is_none() {
        report.push(json!({
            "server": label,
            "pid_file": pid_file,
            "status": "no_pid_file_or_invalid"
        }));
        return Ok(());
    }

    let pid = pid.unwrap_or_default();
    let mut status = "stale_pid";
    let mut killed = false;
    let mut signal = "none";
    let mut error_msg: Option<String> = None;

    if process_is_alive(pid) {
        status = "running";
        if cli.dry_run {
            signal = "SIGTERM(dry_run)";
        } else {
            signal = "SIGTERM";
            if let Err(e) = send_signal(pid, libc::SIGTERM) {
                status = "signal_failed";
                error_msg = Some(format!("SIGTERM failed: {e}"));
            }

            if status != "signal_failed" {
                let deadline = SystemTime::now() + Duration::from_secs(2);
                while SystemTime::now() < deadline {
                    if !process_is_alive(pid) {
                        killed = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }

                if !killed && process_is_alive(pid) {
                    signal = "SIGKILL";
                    if let Err(e) = send_signal(pid, libc::SIGKILL) {
                        status = "signal_failed";
                        error_msg = Some(format!("SIGKILL failed: {e}"));
                    } else {
                        killed = true;
                    }
                }
            }
        }
    }

    if !cli.dry_run && pid_file.exists() {
        let _ = fs::remove_file(pid_file);
    }

    report.push(json!({
        "server": label,
        "pid": pid,
        "pid_file": pid_file,
        "status": status,
        "signal": signal,
        "killed": killed,
        "error": error_msg,
    }));
    Ok(())
}

fn cmd_kill_server(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;

    let mut report = Vec::new();
    kill_server_from_pid_file(cli, "web", &web_server_pid_file(), &mut report)?;
    kill_server_from_pid_file(cli, "parakeet", &parakeet_server_pid_file(), &mut report)?;

    if !cli.quiet {
        for item in &report {
            let server = item
                .get("server")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let pid = item
                .get("pid")
                .and_then(|v| v.as_i64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string());
            let signal = item
                .get("signal")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            println!(
                "kill-server {}: status={} pid={} signal={}",
                server, status, pid, signal
            );
        }
    }

    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "kill-server",
            "servers": report,
        }),
    );

    Ok(0)
}

fn cmd_screenshot_use(cli: &Cli, args: &ScreenshotUseArgs) -> Result<i32, AppError> {
    ensure_dirs()?;
    let session_dir = resolve_session_dir_by_id(&args.session_id)?;
    let events_path = session_dir.join("events.jsonl");
    let events = history::read_jsonl_values(&events_path);
    let shots = reporting::load_shots_for_session(&session_dir, &events);

    let Some(shot) = shots.iter().find(|s| s.shot_id == args.shot_id) else {
        return Err(app_error(
            8,
            format!(
                "Screenshot id {} not found in session {}",
                args.shot_id, args.session_id
            ),
        ));
    };

    let target_path = session_dir.join(&shot.dest_rel_path);
    if !target_path.exists() {
        return Err(app_error(
            8,
            format!(
                "Transcript screenshot path missing: {}",
                target_path.display()
            ),
        ));
    }

    let stem = target_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("shot")
        .to_string();
    let ext = target_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("png")
        .to_string();

    let backup_path = target_path.with_file_name(format!("{stem}__original.{ext}"));
    if !backup_path.exists() {
        fs::copy(&target_path, &backup_path).map_err(|e| {
            app_error(
                1,
                format!(
                    "Failed to create original backup {}: {e}",
                    backup_path.display()
                ),
            )
        })?;
    }

    let normalized_module = args.module.trim().to_ascii_lowercase();
    let source_path = if normalized_module == "original" {
        backup_path.clone()
    } else {
        session_dir
            .join("screenshots")
            .join("derived")
            .join(format!(
                "shot-{:03}__{}.png",
                args.shot_id, normalized_module
            ))
    };

    if !source_path.exists() {
        let _ = generate_html_for_session(&session_dir)?;
    }
    if !source_path.exists() {
        return Err(app_error(
            8,
            format!(
                "Derived screenshot module '{}' not found for shot {} (expected: {})",
                normalized_module,
                args.shot_id,
                source_path.display()
            ),
        ));
    }

    fs::copy(&source_path, &target_path).map_err(|e| {
        app_error(
            1,
            format!(
                "Failed to copy source image {} -> {}: {e}",
                source_path.display(),
                target_path.display()
            ),
        )
    })?;

    let _ = generate_html_for_session(&session_dir)?;
    let _ = generate_sessions_index_html()?;

    print_out(
        cli,
        format!(
            "Set screenshot {} module '{}' as transcript image\npath: {}\nbackup: {}",
            args.shot_id,
            normalized_module,
            target_path.display(),
            backup_path.display()
        ),
    );
    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "screenshot_use",
            "session_id": args.session_id,
            "shot_id": args.shot_id,
            "module": normalized_module,
            "target_path": target_path,
            "backup_path": backup_path,
            "source_path": source_path,
        }),
    );

    Ok(0)
}

fn cmd_toggle(cli: &Cli, args: &ToggleArgs) -> Result<i32, AppError> {
    let active = active_state_file().exists();
    if active {
        let stop_args = StopArgs {
            no_stop_hooks: args.no_stop_hooks,
            transcribe_cmd: args.transcribe_cmd.clone(),
            post_transcribe_cmd: args.post_transcribe_cmd.clone(),
            python_bin: args.python_bin.clone(),
            parakeet_script: args.parakeet_script.clone(),
            parakeet_model: args.parakeet_model.clone(),
        };
        cmd_stop(cli, &stop_args)
    } else {
        let start_args = StartArgs {
            screenshot_dir: args.screenshot_dir.clone(),
            audio_device: args.audio_device.clone(),
        };
        cmd_start(cli, &start_args)
    }
}

fn cmd_fork(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;
    let active = active_state_file();
    if !active.exists() {
        print_out(cli, "No active session.");
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "fork",
                "active": false,
                "message": "No active session."
            }),
        );
        return Ok(0);
    }

    let old_state = load_active_state()?;
    if cli.dry_run {
        print_out(
            cli,
            format!(
                "[dry-run] Would fork active session {} into a new session.",
                old_state.session_id
            ),
        );
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "fork",
                "old_session_id": old_state.session_id,
                "dry_run": true
            }),
        );
        return Ok(0);
    }

    let split_start = Instant::now();
    if let Some(pid) = old_state.ffmpeg_pid {
        if old_state.transcription_paused && process_is_alive(pid) {
            let _ = resume_recorder_capture(pid, cli);
            thread::sleep(Duration::from_millis(20));
        }
        stop_recorder(pid, cli)?;
    }
    if let Some(pid) = old_state.clipboard_watcher_pid {
        stop_clipboard_watcher(pid, cli);
    }
    let split_gap_ms = split_start.elapsed().as_secs_f64() * 1000.0;

    clear_active_state()?;
    let start_args = StartArgs {
        screenshot_dir: Some(PathBuf::from(old_state.screenshot_source_dir.clone())),
        audio_device: old_state.audio_device.clone(),
    };
    let internal_cli = Cli {
        verbose: cli.verbose,
        quiet: true,
        json: false,
        dry_run: false,
        no_beeps: true,
        command: Commands::Status,
    };
    cmd_start(&internal_cli, &start_args)?;
    let new_state = load_active_state()?;
    let split_to_running_ms = split_start.elapsed().as_secs_f64() * 1000.0;

    write_json(&active_state_file(), &old_state)?;
    let stop_args = StopArgs {
        no_stop_hooks: false,
        transcribe_cmd: None,
        post_transcribe_cmd: None,
        python_bin: None,
        parakeet_script: None,
        parakeet_model: None,
    };
    let finalize_result = cmd_stop(&internal_cli, &stop_args);
    let restore_result = save_active_state(&new_state);
    if let Err(e) = restore_result {
        return Err(app_error(
            1,
            format!(
                "Forked to new session {}, but failed to restore active state: {}",
                new_state.session_id, e
            ),
        ));
    }
    if let Err(e) = finalize_result {
        return Err(app_error(
            e.code,
            format!(
                "Forked to new session {}, but failed finalizing old session {}: {}",
                new_state.session_id, old_state.session_id, e.message
            ),
        ));
    }

    print_out(
        cli,
        format!(
            "Forked session {}\nnew session {}\nsplit_gap_ms: {}\nsplit_to_running_ms: {}",
            old_state.session_id,
            new_state.session_id,
            round3(split_gap_ms),
            round3(split_to_running_ms)
        ),
    );
    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "fork",
            "old_session_id": old_state.session_id,
            "new_session_id": new_state.session_id,
            "split_gap_ms": round3(split_gap_ms),
            "split_to_running_ms": round3(split_to_running_ms),
            "new_session_dir": new_state.session_dir
        }),
    );
    Ok(0)
}

fn cmd_toggle_pause(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;
    let active = active_state_file();
    if !active.exists() {
        return cmd_pause(cli);
    }
    let state: SessionState = read_json(&active)?;
    if state.transcription_paused {
        cmd_unpause(cli)
    } else {
        cmd_pause(cli)
    }
}

fn run(cli: &Cli) -> Result<i32, AppError> {
    match &cli.command {
        Commands::Start(args) => cmd_start(cli, args),
        Commands::Shot => cmd_shot(cli),
        Commands::Stop(args) => cmd_stop(cli, args),
        Commands::Toggle(args) => cmd_toggle(cli, args),
        Commands::Fork => cmd_fork(cli),
        Commands::Live(args) => cmd_live(cli, args),
        Commands::Chunk => cmd_chunk(cli),
        Commands::Pause => cmd_pause(cli),
        Commands::Unpause => cmd_unpause(cli),
        Commands::TogglePause => cmd_toggle_pause(cli),
        Commands::Sounds => cmd_sounds(cli),
        Commands::Silence => cmd_silence(cli),
        Commands::Loud => cmd_loud(cli),
        Commands::Status => cmd_status(cli),
        Commands::Perf(args) => cmd_perf(cli, args),
        Commands::List(args) => cmd_list(cli, args),
        Commands::Copy(args) => cmd_copy(cli, args),
        Commands::Send(args) => cmd_send(cli, args),
        Commands::SendImages(args) => cmd_send_images(cli, args),
        Commands::Show(args) => cmd_show(cli, args),
        Commands::Html(args) => cmd_html(cli, args),
        Commands::ScreenshotUse(args) => cmd_screenshot_use(cli, args),
        Commands::WatchClipboard(args) => cmd_watch_clipboard(cli, args),
        Commands::KillServer => cmd_kill_server(cli),
    }
}

fn main() {
    let original_env_keys: HashSet<OsString> = env::vars_os().map(|(k, _)| k).collect();
    load_riffrc_defaults(&original_env_keys);
    load_riff_json_defaults(&original_env_keys);
    let cli = Cli::parse();
    let exit = match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            if cli.json {
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": false,
                        "error": err.message,
                        "code": err.code
                    }))
                    .unwrap_or_else(|_| "{\"ok\":false}".to_string())
                );
            } else {
                eprintln!("Error: {}", err.message);
            }
            err.code
        }
    };
    std::process::exit(exit);
}

#[cfg(test)]
mod tests {
    use super::{
        expand_env_refs, fill_template_with_transcript, load_riff_json_defaults,
        parse_riffrc_assignment,
    };
    use serde_json::json;
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn parse_riffrc_accepts_export_and_quotes() {
        let parsed = parse_riffrc_assignment(
            r#"export RIFF_PARAKEET_SCRIPT="/Users/test/Code/riff/scripts/parakeet_transcribe.py""#,
        )
        .expect("parse export");
        assert_eq!(parsed.0, "RIFF_PARAKEET_SCRIPT");
        assert_eq!(
            parsed.1,
            "/Users/test/Code/riff/scripts/parakeet_transcribe.py"
        );
    }

    #[test]
    fn parse_riffrc_rejects_invalid_key() {
        assert!(parse_riffrc_assignment("1BAD_KEY=value").is_none());
        assert!(parse_riffrc_assignment("export =value").is_none());
    }

    #[test]
    fn expand_env_refs_handles_home_style_tokens() {
        let home = std::env::var("HOME").unwrap_or_default();
        let expanded = expand_env_refs("${HOME}/Code/riff:$HOME/bin");
        assert_eq!(expanded, format!("{home}/Code/riff:{home}/bin"));
    }

    #[test]
    fn fill_template_with_transcript_shell_escapes_transcript() {
        let rendered = fill_template_with_transcript(
            "agent --text {transcript}",
            Path::new("/tmp/audio.wav"),
            Path::new("/tmp/out"),
            Path::new("/tmp/out.txt"),
            Path::new("/tmp/session"),
            Some("hello \"quoted\" world"),
        );
        assert_eq!(rendered, "agent --text \"hello \\\"quoted\\\" world\"");
    }

    #[test]
    fn json_config_sets_post_transcribe_default() {
        let td = tempdir().expect("tempdir");
        let json_path = td.path().join("riff.json");
        fs::write(
            &json_path,
            serde_json::to_string(&json!({
                "riff": {
                    "post_transcribe_cmd": "agent --text {transcript}"
                }
            }))
            .expect("serialize json"),
        )
        .expect("write config");

        let original = std::env::var_os("RIFF_POST_TRANSCRIBE_CMD");
        let original_path = std::env::var_os("RIFF_CONFIG_JSON_FILE");
        std::env::remove_var("RIFF_POST_TRANSCRIBE_CMD");
        std::env::set_var("RIFF_CONFIG_JSON_FILE", &json_path);

        let original_env_keys: HashSet<OsString> = std::env::vars_os().map(|(k, _)| k).collect();
        load_riff_json_defaults(&original_env_keys);

        assert_eq!(
            std::env::var("RIFF_POST_TRANSCRIBE_CMD").ok().as_deref(),
            Some("agent --text {transcript}")
        );

        if let Some(value) = original {
            std::env::set_var("RIFF_POST_TRANSCRIBE_CMD", value);
        } else {
            std::env::remove_var("RIFF_POST_TRANSCRIBE_CMD");
        }
        if let Some(value) = original_path {
            std::env::set_var("RIFF_CONFIG_JSON_FILE", value);
        } else {
            std::env::remove_var("RIFF_CONFIG_JSON_FILE");
        }
    }
}
