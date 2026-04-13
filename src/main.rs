use chrono::{SecondsFormat, Utc};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
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
mod session_commands;

use crate::cli::{Cli, Commands, HtmlArgs, StopArgs};
use crate::error::{app_error, AppError};
use crate::history::{cmd_copy, cmd_list, cmd_show, resolve_recent_session_dir};
use crate::models::{SessionState, ShotMeta};
use crate::paths::{
    active_state_file, audio_device_cache_file, ensure_dirs, parakeet_server_log_file,
    parakeet_server_pid_file, perf_log_file, root_dir, web_server_log_file, web_server_pid_file,
};
use crate::reporting::generate_html_for_session;
use crate::session_commands::{cmd_shot, cmd_start, cmd_stop};

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

fn print_verbose(cli: &Cli, message: impl AsRef<str>) {
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

fn command_exists(cmd: &str) -> bool {
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
                print_verbose(cli, format!("Using cached audio device {}", cached));
                return cached.to_string();
            }
        }
    }

    let devices = list_avfoundation_audio_devices();
    if devices.is_empty() {
        print_verbose(
            cli,
            "Could not auto-detect macOS audio devices; falling back to :0",
        );
        return ":0".to_string();
    }

    let avoid = ["iphone", "continuity"];
    let built_in = ["macbook", "built-in", "internal"];

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

fn process_is_alive(pid: i32) -> bool {
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

fn read_pid_file(path: &Path) -> Option<i32> {
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse::<i32>().ok()
}

fn write_pid_file(path: &Path, pid: i32) {
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

fn shell_escape(text: &str) -> String {
    if text.is_empty() {
        return "''".to_string();
    }
    let escaped = text.replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

fn fill_template(
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

fn default_parakeet_script() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("scripts/parakeet_transcribe.py"));
    }

    if let Ok(exe) = env::current_exe() {
        let mut parent = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..5 {
            if let Some(p) = parent.clone() {
                candidates.push(p.join("scripts/parakeet_transcribe.py"));
                parent = p.parent().map(|x| x.to_path_buf());
            }
        }
    }

    candidates.into_iter().find(|p| p.exists())
}

fn parakeet_server_enabled() -> bool {
    env::var("ISPY_PARAKEET_SERVER")
        .map(|v| {
            !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true)
}

fn parakeet_server_base_url() -> String {
    env::var("ISPY_PARAKEET_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:8765".to_string())
}

fn parakeet_server_health_url(base: &str) -> String {
    format!("{}/health", base.trim_end_matches('/'))
}

fn parakeet_server_transcribe_url(base: &str) -> String {
    format!("{}/transcribe", base.trim_end_matches('/'))
}

fn check_parakeet_server_health(base_url: &str) -> bool {
    if !command_exists("curl") {
        return false;
    }

    let out = Command::new("curl")
        .args(["-sS", "--max-time", "0.5", "--fail"])
        .arg(parakeet_server_health_url(base_url))
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let body = String::from_utf8_lossy(&o.stdout).to_string();
            body.contains("\"ok\": true") || body.contains("\"ok\":true")
        }
        _ => false,
    }
}

fn spawn_parakeet_server(
    python_bin: &str,
    script_path: &Path,
    model: &str,
    cli: &Cli,
) -> Result<(), AppError> {
    let pid_file = parakeet_server_pid_file();
    if let Some(pid) = read_pid_file(&pid_file) {
        if process_is_alive(pid) {
            print_verbose(
                cli,
                format!("Parakeet server process already running (pid={})", pid),
            );
            return Ok(());
        }
    }

    let base_url = parakeet_server_base_url();
    let host_port = base_url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let mut parts = host_port.split(':');
    let host = parts.next().unwrap_or("127.0.0.1");
    let port = parts.next().unwrap_or("8765");

    let log_path = parakeet_server_log_file();
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| app_error(1, format!("Failed to open {}: {e}", log_path.display())))?;
    let log_file_err = log_file
        .try_clone()
        .map_err(|e| app_error(1, format!("Failed to clone server log file handle: {e}")))?;

    print_verbose(
        cli,
        format!(
            "Starting Parakeet server at {} using model {}",
            base_url, model
        ),
    );

    let child = Command::new(python_bin)
        .arg(script_path)
        .arg("--serve")
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port)
        .arg("--model")
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
        .map_err(|e| app_error(1, format!("Failed to start Parakeet server: {e}")))?;

    write_pid_file(&pid_file, child.id() as i32);
    Ok(())
}

fn ensure_parakeet_server(
    python_bin: &str,
    script_path: &Path,
    model: &str,
    cli: &Cli,
    wait_ready: bool,
) {
    if !parakeet_server_enabled() {
        return;
    }

    let base_url = parakeet_server_base_url();
    if check_parakeet_server_health(&base_url) {
        return;
    }

    let _ = spawn_parakeet_server(python_bin, script_path, model, cli);
    if !wait_ready {
        return;
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if check_parakeet_server_health(&base_url) {
            print_verbose(cli, format!("Parakeet server ready at {}", base_url));
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
    print_verbose(
        cli,
        format!(
            "Parakeet server not ready yet at {} (will fallback)",
            base_url
        ),
    );
}

fn web_server_enabled() -> bool {
    env::var("ISPY_WEB_SERVER")
        .map(|v| {
            !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true)
}

fn web_server_base_url() -> String {
    env::var("ISPY_WEB_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:8766".to_string())
}

fn web_server_idle_timeout_sec() -> u64 {
    env::var("ISPY_WEB_SERVER_IDLE_TIMEOUT_SEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1800)
}

fn default_web_server_script() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("scripts/ispy_web_server.py"));
    }

    if let Ok(exe) = env::current_exe() {
        let mut parent = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..5 {
            if let Some(p) = parent.clone() {
                candidates.push(p.join("scripts/ispy_web_server.py"));
                parent = p.parent().map(|x| x.to_path_buf());
            }
        }
    }

    candidates.into_iter().find(|p| p.exists())
}

fn default_sound_picker_script() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("scripts/pick_ispy_sounds.sh"));
    }

    if let Ok(exe) = env::current_exe() {
        let mut parent = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..5 {
            if let Some(p) = parent.clone() {
                candidates.push(p.join("scripts/pick_ispy_sounds.sh"));
                parent = p.parent().map(|x| x.to_path_buf());
            }
        }
    }

    candidates.into_iter().find(|p| p.exists())
}

fn web_server_health_url(base: &str) -> String {
    format!("{}/health", base.trim_end_matches('/'))
}

fn web_server_touch_url(base: &str) -> String {
    format!("{}/touch", base.trim_end_matches('/'))
}

fn check_web_server_health(base_url: &str) -> bool {
    if !command_exists("curl") {
        return false;
    }

    let out = Command::new("curl")
        .args(["-sS", "--max-time", "0.5", "--fail"])
        .arg(web_server_health_url(base_url))
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let body = String::from_utf8_lossy(&o.stdout).to_string();
            body.contains("\"ok\": true") || body.contains("\"ok\":true")
        }
        _ => false,
    }
}

fn touch_web_server(base_url: &str) -> bool {
    if !command_exists("curl") {
        return false;
    }

    let out = Command::new("curl")
        .args(["-sS", "--max-time", "0.5", "--fail", "-X", "POST"])
        .arg(web_server_touch_url(base_url))
        .output();

    match out {
        Ok(o) if o.status.success() => true,
        _ => false,
    }
}

fn spawn_web_server(python_bin: &str, script_path: &Path, cli: &Cli) -> Result<(), AppError> {
    let pid_file = web_server_pid_file();
    if let Some(pid) = read_pid_file(&pid_file) {
        if process_is_alive(pid) {
            print_verbose(
                cli,
                format!("Web server process already running (pid={})", pid),
            );
            return Ok(());
        }
    }

    let base_url = web_server_base_url();
    let host_port = base_url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let mut parts = host_port.split(':');
    let host = parts.next().unwrap_or("127.0.0.1");
    let port = parts.next().unwrap_or("8766");

    let log_path = web_server_log_file();
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| app_error(1, format!("Failed to open {}: {e}", log_path.display())))?;
    let log_file_err = log_file.try_clone().map_err(|e| {
        app_error(
            1,
            format!("Failed to clone web server log file handle: {e}"),
        )
    })?;

    print_verbose(
        cli,
        format!(
            "Starting web server at {} (idle timeout {}s)",
            base_url,
            web_server_idle_timeout_sec()
        ),
    );

    let child = Command::new(python_bin)
        .arg(script_path)
        .arg("--root")
        .arg(root_dir())
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port)
        .arg("--idle-timeout-sec")
        .arg(web_server_idle_timeout_sec().to_string())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
        .map_err(|e| app_error(1, format!("Failed to start web server: {e}")))?;

    write_pid_file(&pid_file, child.id() as i32);
    Ok(())
}

fn ensure_web_server(cli: &Cli, wait_ready: bool) -> bool {
    if !web_server_enabled() {
        return false;
    }

    let base_url = web_server_base_url();
    if check_web_server_health(&base_url) {
        return true;
    }

    let python_bin = env::var("ISPY_PYTHON_BIN").unwrap_or_else(|_| "python3".to_string());
    let Some(script_path) = env::var("ISPY_WEB_SERVER_SCRIPT")
        .ok()
        .map(PathBuf::from)
        .or_else(default_web_server_script)
    else {
        print_verbose(
            cli,
            "No web server script found; skipping auto web server startup.",
        );
        return false;
    };

    if spawn_web_server(&python_bin, &script_path, cli).is_err() {
        return false;
    }

    if !wait_ready {
        return false;
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check_web_server_health(&base_url) {
            return true;
        }
        thread::sleep(Duration::from_millis(150));
    }

    false
}

fn transcribe_via_parakeet_server(
    base_url: &str,
    audio_path: &Path,
    out_txt: &Path,
    model: &str,
) -> Result<(String, Value), Value> {
    if !command_exists("curl") {
        return Err(json!({
            "status": "error",
            "method": "parakeet_server",
            "reason": "curl not found"
        }));
    }

    let payload = json!({
        "audio": audio_path,
        "out_txt": out_txt,
        "model": model,
        "batch_size": 1
    })
    .to_string();

    let out = Command::new("curl")
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
        .arg(parakeet_server_transcribe_url(base_url))
        .output();

    let out = match out {
        Ok(o) => o,
        Err(e) => {
            return Err(json!({
                "status": "error",
                "method": "parakeet_server",
                "reason": format!("curl failed: {e}")
            }))
        }
    };

    if !out.status.success() {
        return Err(json!({
            "status": "error",
            "method": "parakeet_server",
            "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string(),
            "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(),
            "returncode": out.status.code(),
        }));
    }

    let body = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return Err(json!({
                "status": "error",
                "method": "parakeet_server",
                "reason": format!("invalid JSON response: {e}"),
                "body": body,
            }))
        }
    };

    if parsed.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(json!({
            "status": "error",
            "method": "parakeet_server",
            "response": parsed,
        }));
    }

    let txt = parsed
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    Ok((
        txt,
        json!({
            "status": "ok",
            "method": "parakeet_server",
            "server": base_url,
            "model": model,
            "elapsed_sec": parsed.get("elapsed_sec").and_then(|v| v.as_f64()),
        }),
    ))
}

fn run_transcription(
    state: &SessionState,
    session_dir: &Path,
    stop_args: &StopArgs,
    cli: &Cli,
) -> (String, Value) {
    if cli.dry_run {
        return (
            String::new(),
            json!({"status": "dry_run", "reason": "transcription skipped"}),
        );
    }

    let audio_path = PathBuf::from(&state.audio_path);
    if !audio_path.exists() {
        return (
            String::new(),
            json!({
                "status": "missing_audio",
                "reason": format!("Audio file not found: {}", audio_path.display())
            }),
        );
    }

    let out_base = session_dir.join("transcript");
    let out_txt = session_dir.join("transcript.txt");

    let cmd_template = stop_args
        .transcribe_cmd
        .clone()
        .or_else(|| env::var("ISPY_TRANSCRIBE_CMD").ok());

    if let Some(template) = cmd_template {
        let filled = fill_template(&template, &audio_path, &out_base, &out_txt, session_dir);
        print_verbose(cli, format!("Running transcription command: {filled}"));

        let output = Command::new("sh").arg("-lc").arg(&filled).output();
        match output {
            Ok(out) if out.status.success() => {
                let txt = if out_txt.exists() {
                    fs::read_to_string(&out_txt).unwrap_or_default()
                } else {
                    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                    if !stdout.trim().is_empty() {
                        let _ = fs::write(&out_txt, stdout.as_bytes());
                    }
                    stdout
                };
                return (
                    txt.trim().to_string(),
                    json!({"status": "ok", "method": "custom_command", "cmd": filled}),
                );
            }
            Ok(out) => {
                return (
                    String::new(),
                    json!({
                        "status": "error",
                        "method": "custom_command",
                        "cmd": filled,
                        "returncode": out.status.code(),
                        "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string()
                    }),
                );
            }
            Err(e) => {
                return (
                    String::new(),
                    json!({
                        "status": "error",
                        "method": "custom_command",
                        "cmd": filled,
                        "reason": format!("Failed to spawn shell command: {e}")
                    }),
                );
            }
        }
    }

    let script = stop_args
        .parakeet_script
        .clone()
        .or_else(|| env::var("ISPY_PARAKEET_SCRIPT").ok().map(PathBuf::from))
        .or_else(default_parakeet_script);

    let Some(script_path) = script else {
        return (
            String::new(),
            json!({
                "status": "skipped",
                "reason": "No transcription configured. Set --parakeet-script or ISPY_PARAKEET_SCRIPT, or use --transcribe-cmd."
            }),
        );
    };

    let python_bin = stop_args
        .python_bin
        .clone()
        .or_else(|| env::var("ISPY_PYTHON_BIN").ok())
        .unwrap_or_else(|| "python3".to_string());

    let model = stop_args
        .parakeet_model
        .clone()
        .or_else(|| env::var("ISPY_PARAKEET_MODEL").ok())
        .unwrap_or_else(|| "nvidia/parakeet-tdt-0.6b-v2".to_string());

    if parakeet_server_enabled() {
        let base_url = parakeet_server_base_url();
        ensure_parakeet_server(&python_bin, &script_path, &model, cli, true);
        if check_parakeet_server_health(&base_url) {
            match transcribe_via_parakeet_server(&base_url, &audio_path, &out_txt, &model) {
                Ok((txt, meta)) => {
                    if !txt.is_empty() {
                        let _ = fs::write(&out_txt, format!("{}\n", txt));
                    }
                    return (txt, meta);
                }
                Err(meta) => {
                    print_verbose(
                        cli,
                        format!(
                            "Parakeet server transcription failed, falling back to one-shot process: {}",
                            meta
                        ),
                    );
                }
            }
        }
    }

    let cmd_for_log = format!(
        "{} {} --audio {} --out-txt {} --model {}",
        shell_escape(&python_bin),
        shell_escape(&script_path.display().to_string()),
        shell_escape(&audio_path.display().to_string()),
        shell_escape(&out_txt.display().to_string()),
        shell_escape(&model)
    );

    print_verbose(
        cli,
        format!("Running Parakeet transcription (one-shot): {cmd_for_log}"),
    );

    let output = Command::new(&python_bin)
        .arg(&script_path)
        .arg("--audio")
        .arg(&audio_path)
        .arg("--out-txt")
        .arg(&out_txt)
        .arg("--model")
        .arg(&model)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let txt = if out_txt.exists() {
                fs::read_to_string(&out_txt).unwrap_or_default()
            } else {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                if !stdout.trim().is_empty() {
                    let _ = fs::write(&out_txt, stdout.as_bytes());
                }
                stdout
            };

            (
                txt.trim().to_string(),
                json!({
                    "status": "ok",
                    "method": "parakeet_python",
                    "cmd": cmd_for_log,
                    "script": script_path,
                    "model": model,
                }),
            )
        }
        Ok(out) => (
            String::new(),
            json!({
                "status": "error",
                "method": "parakeet_python",
                "cmd": cmd_for_log,
                "returncode": out.status.code(),
                "signal": out.status.signal(),
                "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string(),
                "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(),
            }),
        ),
        Err(e) => (
            String::new(),
            json!({
                "status": "error",
                "method": "parakeet_python",
                "cmd": cmd_for_log,
                "reason": format!("Failed to run python transcription: {e}")
            }),
        ),
    }
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

    print_out(
        cli,
        format!(
            "Active session: {}\nsession_dir: {}\nffmpeg_pid: {} (alive={})",
            state.session_id,
            state.session_dir,
            pid.map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string()),
            alive
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
        }),
    );

    Ok(0)
}

fn cmd_html(cli: &Cli, args: &HtmlArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let requested_rank = args.n.unwrap_or(1);
    let session_dir = resolve_recent_session_dir(requested_rank)?;

    // Always regenerate so HTML reflects latest template/features.
    let html_path = generate_html_for_session(&session_dir)?;

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
            "opened": true,
            "opened_target": opened_target,
            "web_server_ready": server_ready,
            "web_server_url": if server_ready { Value::String(base_url) } else { Value::Null },
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
