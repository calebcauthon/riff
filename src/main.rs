use chrono::{SecondsFormat, Utc};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod cli;
mod error;
mod history;
mod models;
mod paths;
mod reporting;
mod session_commands;
mod transcription;

use crate::cli::{Cli, Commands, HtmlArgs, WatchClipboardArgs};
use crate::error::{app_error, AppError};
use crate::history::{
    cmd_copy, cmd_list, cmd_show, resolve_recent_session_dir, resolve_session_dir_by_id,
};
use crate::models::{SessionState, ShotMeta};
use crate::paths::{
    active_state_file, audio_device_cache_file, ensure_dirs, parakeet_server_pid_file,
    perf_log_file, web_server_pid_file,
};
use crate::reporting::{generate_html_for_session, generate_sessions_index_html};
use crate::session_commands::{cmd_shot, cmd_start, cmd_stop};
use crate::transcription::{
    default_sound_picker_script, ensure_web_server, touch_web_server, web_server_base_url,
};

pub(crate) const SUPPORTED_IMAGE_EXTS: &[&str] =
    &["png", "jpg", "jpeg", "webp", "tif", "tiff", "heic", "heif"];

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
    bool_env_enabled("ISPY_CLIPBOARD_MONITOR", true)
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
        print_verbose(cli, "Clipboard watcher disabled by ISPY_CLIPBOARD_MONITOR.");
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
    let deadline = SystemTime::now() + Duration::from_millis(800);
    while SystemTime::now() < deadline {
        if !process_is_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(40));
    }
    let _ = send_signal(pid, libc::SIGKILL);
    print_verbose(
        cli,
        format!("Clipboard watcher pid={pid} was force-stopped."),
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
        "ISPY_BEEP_START_COUNT"
    } else {
        "ISPY_BEEP_STOP_COUNT"
    };

    let parsed = env::var(key)
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(1);

    parsed.clamp(1, 3)
}

fn env_beep_gap_sec() -> f32 {
    let parsed = env::var("ISPY_BEEP_GAP_SEC")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(0.08);

    parsed.clamp(0.0, 1.0)
}

fn play_event_sound(kind: &str, cli: &Cli) {
    if !bool_env_enabled("ISPY_BEEP", true) {
        return;
    }

    let env_key = if kind == "start" {
        "ISPY_BEEP_START"
    } else {
        "ISPY_BEEP_STOP"
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
            .arg("ispy-beep")
            .arg(count.to_string())
            .arg(sound_path.as_os_str())
            .arg(format!("{:.2}", gap_sec))
            .spawn();
        return;
    }

    if command_exists("osascript") {
        let script = format!("beep {}", count);
        let _ = Command::new("osascript").args(["-e", &script]).spawn();
        if cli.verbose && !cli.quiet {
            eprintln!("[verbose] fallback beep used for {} x{}", kind, count);
        }
    }
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

    let avoid = ["iphone", "continuity"];
    let built_in = ["macbook", "built-in", "internal"];
    let devices = list_avfoundation_audio_devices();
    let cache_file = audio_device_cache_file();

    if use_cache {
        if let Ok(cached) = fs::read_to_string(&cache_file) {
            let cached = cached.trim();
            if cached.starts_with(':') && cached.len() > 1 {
                if devices.is_empty() {
                    print_verbose(
                        cli,
                        format!(
                            "Using cached audio device {} (device list unavailable)",
                            cached
                        ),
                    );
                    return cached.to_string();
                }

                let cached_idx = &cached[1..];
                if let Some((_, name)) = devices.iter().find(|(idx, _)| idx == cached_idx) {
                    let lc = name.to_ascii_lowercase();
                    if !contains_any(&lc, &avoid) {
                        print_verbose(
                            cli,
                            format!("Using cached audio device {} ({})", cached, name),
                        );
                        return cached.to_string();
                    }
                    print_verbose(
                        cli,
                        format!(
                            "Ignoring cached audio device {} ({}), matched avoided device class.",
                            cached, name
                        ),
                    );
                    let _ = fs::remove_file(&cache_file);
                }
            }
        }
    }

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

fn detect_screenshot_dir(explicit: Option<&Path>, cli: &Cli) -> Result<PathBuf, AppError> {
    if let Some(p) = explicit {
        let expanded = expand_tilde(p);
        if expanded.is_dir() {
            return Ok(expanded);
        }
        return Err(app_error(
            3,
            format!(
                "Screenshot directory does not exist: {}",
                expanded.display()
            ),
        ));
    }

    let defaults = Command::new("defaults")
        .args(["read", "com.apple.screencapture", "location"])
        .output();

    if let Ok(out) = defaults {
        if out.status.success() {
            let candidate = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !candidate.is_empty() {
                let p = expand_tilde(Path::new(&candidate));
                if p.is_dir() {
                    print_verbose(
                        cli,
                        format!("Detected screenshot dir from defaults: {}", p.display()),
                    );
                    return Ok(p);
                }
            }
        }
    }

    let fallback = home_dir().join("Desktop");
    if fallback.is_dir() {
        print_verbose(
            cli,
            format!("Falling back to screenshot dir: {}", fallback.display()),
        );
        return Ok(fallback);
    }

    Err(app_error(
        3,
        format!(
            "Could not detect screenshot directory from macOS defaults or fallback {}",
            fallback.display()
        ),
    ))
}

fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    path.to_path_buf()
}

fn home_dir() -> PathBuf {
    env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/Users/unknown"))
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
        return Err(app_error(
            4,
            "No active session. Run 'dictate start' first.",
        ));
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
        thread::sleep(Duration::from_millis(50));
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

    let started = wait_for_process_start(&mut child, Duration::from_millis(300))?;
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

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub(crate) fn file_mtime_epoch(path: &Path) -> Option<f64> {
    let md = fs::metadata(path).ok()?;
    let modified = md.modified().ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

fn find_session_screenshots(
    source_dir: &Path,
    started_epoch: f64,
    ended_epoch: f64,
) -> Vec<(PathBuf, f64)> {
    let mut files = Vec::new();

    let entries = match fs::read_dir(source_dir) {
        Ok(e) => e,
        Err(_) => return files,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if !SUPPORTED_IMAGE_EXTS.contains(&ext.as_str()) {
            continue;
        }

        let Some(mtime) = file_mtime_epoch(&path) else {
            continue;
        };

        if (started_epoch - 1.0..=ended_epoch + 2.0).contains(&mtime) {
            files.push((path, mtime));
        }
    }

    files.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    files
}

fn move_session_screenshots(
    source_dir: &Path,
    target_dir: &Path,
    started_epoch: f64,
    ended_epoch: f64,
    events_path: &Path,
    start_index: usize,
    cli: &Cli,
) -> Result<Vec<ShotMeta>, AppError> {
    let mut out = Vec::new();
    let shots = find_session_screenshots(source_dir, started_epoch, ended_epoch);

    for (index, (source, mtime)) in shots.into_iter().enumerate() {
        let shot_id = start_index + index + 1;
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| "png".to_string());
        let dest_name = format!("shot-{shot_id:03}.{ext}");
        let dest_abs = target_dir.join(&dest_name);
        let dest_rel = format!("screenshots/{dest_name}");
        let audio_sec = (mtime - started_epoch).max(0.0);

        if cli.dry_run {
            print_out(
                cli,
                format!(
                    "[dry-run] Would copy {} -> {} and delete source",
                    source.display(),
                    dest_abs.display()
                ),
            );
        } else {
            fs::copy(&source, &dest_abs).map_err(|e| {
                app_error(
                    1,
                    format!(
                        "Failed to copy screenshot {} -> {}: {e}",
                        source.display(),
                        dest_abs.display()
                    ),
                )
            })?;
            fs::remove_file(&source).map_err(|e| {
                app_error(
                    1,
                    format!("Failed to delete screenshot {}: {e}", source.display()),
                )
            })?;

            append_jsonl(
                events_path,
                &json!({
                    "ts": now_iso(),
                    "type": "screenshot_moved",
                    "id": shot_id,
                    "source": source,
                    "dest": dest_rel,
                    "audioSec": round3(audio_sec),
                    "mtime_epoch": round3(mtime),
                }),
            )?;
        }

        out.push(ShotMeta {
            shot_id,
            dest_rel_path: dest_rel,
            audio_sec,
        });
    }

    Ok(out)
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
    let mut s = template.to_string();
    s = s.replace("{audio}", &shell_escape(&audio.display().to_string()));
    s = s.replace("{out_base}", &shell_escape(&out_base.display().to_string()));
    s = s.replace("{out_txt}", &shell_escape(&out_txt.display().to_string()));
    s = s.replace(
        "{session_dir}",
        &shell_escape(&session_dir.display().to_string()),
    );
    s
}

pub(crate) fn get_audio_duration_sec(audio_path: &Path) -> Option<f64> {
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

fn cmd_sounds(_cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;

    let script_path = env::var("ISPY_SOUND_PICKER_SCRIPT")
        .ok()
        .map(PathBuf::from)
        .or_else(default_sound_picker_script)
        .ok_or_else(|| {
            app_error(
                1,
                "Could not find sound picker script. Expected scripts/pick_ispy_sounds.sh",
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

fn cmd_status(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;
    let active = active_state_file();
    if !active.exists() {
        print_out(cli, "No active session.");
        emit_json(cli, &json!({ "active": false }));
        return Ok(0);
    }

    let state: SessionState = read_json(&active)?;
    let pid = state.ffmpeg_pid;
    let alive = pid.map(process_is_alive).unwrap_or(false);
    let watcher_pid = state.clipboard_watcher_pid;
    let watcher_alive = watcher_pid.map(process_is_alive).unwrap_or(false);

    print_out(
        cli,
        format!(
            "Active session: {}\nsession_dir: {}\nffmpeg_pid: {} (alive={})\nclipboard_watcher_pid: {} (alive={})",
            state.session_id,
            state.session_dir,
            pid.map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string()),
            alive
            ,
            watcher_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string()),
            watcher_alive
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
        }),
    );

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

fn run(cli: &Cli) -> Result<i32, AppError> {
    match &cli.command {
        Commands::Start(args) => cmd_start(cli, args),
        Commands::Shot => cmd_shot(cli),
        Commands::Stop(args) => cmd_stop(cli, args),
        Commands::Sounds => cmd_sounds(cli),
        Commands::Status => cmd_status(cli),
        Commands::List(args) => cmd_list(cli, args),
        Commands::Copy(args) => cmd_copy(cli, args),
        Commands::Show(args) => cmd_show(cli, args),
        Commands::Html(args) => cmd_html(cli, args),
        Commands::WatchClipboard(args) => cmd_watch_clipboard(cli, args),
        Commands::KillServer => cmd_kill_server(cli),
    }
}

fn main() {
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
