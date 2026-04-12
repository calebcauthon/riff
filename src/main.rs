use chrono::{DateTime, Datelike, Local, NaiveDateTime, SecondsFormat, Timelike, Utc};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SUPPORTED_IMAGE_EXTS: &[&str] =
    &["png", "jpg", "jpeg", "webp", "tif", "tiff", "heic", "heif"];

#[derive(Parser, Debug)]
#[command(
    name = "dictate",
    about = "ispy CLI: local dictation + screenshot session tool"
)]
struct Cli {
    #[arg(long, global = true)]
    verbose: bool,

    #[arg(long, global = true)]
    quiet: bool,

    #[arg(long, global = true)]
    json: bool,

    #[arg(long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Start(StartArgs),
    Shot,
    Stop(StopArgs),
    Status,
    Last(LastArgs),
    List(ListArgs),
    Copy(CopyArgs),
    Show(ShowArgs),
    Html(HtmlArgs),
}

#[derive(Args, Debug)]
struct StartArgs {
    #[arg(long)]
    screenshot_dir: Option<PathBuf>,

    #[arg(long, default_value = "auto")]
    audio_device: String,
}

#[derive(Args, Debug)]
struct StopArgs {
    #[arg(long)]
    transcribe_cmd: Option<String>,

    #[arg(long)]
    python_bin: Option<String>,

    #[arg(long)]
    parakeet_script: Option<PathBuf>,

    #[arg(long)]
    parakeet_model: Option<String>,
}

#[derive(Args, Debug)]
struct LastArgs {
    #[arg(long)]
    open: bool,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Number of recent sessions to show
    n: Option<usize>,
}

#[derive(Args, Debug)]
struct CopyArgs {
    /// Which recent session to output (1 = most recent)
    n: Option<usize>,
}

#[derive(Args, Debug)]
struct ShowArgs {
    /// Which recent session to output (1 = most recent)
    n: Option<usize>,
}

#[derive(Args, Debug)]
struct HtmlArgs {
    /// Which recent session to open (1 = most recent)
    n: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionState {
    session_id: String,
    session_dir: String,
    screenshots_dir: String,
    audio_path: String,
    events_path: String,
    ffmpeg_log_path: String,
    ffmpeg_pid: Option<i32>,
    started_at_iso: String,
    started_at_epoch: f64,
    screenshot_source_dir: String,
    audio_device: String,
}

#[derive(Debug, Clone)]
struct ShotMeta {
    shot_id: usize,
    dest_rel_path: String,
    audio_sec: f64,
}

#[derive(Debug, Serialize)]
struct SessionListRow {
    session_id: String,
    timestamp: String,
    summary: String,
    images: usize,
    duration: String,
}

#[derive(Debug)]
struct AppError {
    code: i32,
    message: String,
}

impl Display for AppError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

fn app_error(code: i32, message: impl Into<String>) -> AppError {
    AppError {
        code,
        message: message.into(),
    }
}

fn root_dir() -> PathBuf {
    env::var("ISPY_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/ispy"))
}

fn sessions_dir() -> PathBuf {
    root_dir().join("sessions")
}

fn active_state_file() -> PathBuf {
    root_dir().join("active_session.json")
}

fn last_session_file() -> PathBuf {
    root_dir().join("last_session.json")
}

fn perf_log_file() -> PathBuf {
    root_dir().join("perf.jsonl")
}

fn audio_device_cache_file() -> PathBuf {
    root_dir().join("audio_device_cache.txt")
}

fn parakeet_server_log_file() -> PathBuf {
    root_dir().join("parakeet-server.log")
}

fn parakeet_server_pid_file() -> PathBuf {
    root_dir().join("parakeet-server.pid")
}

fn ensure_dirs() -> Result<(), AppError> {
    fs::create_dir_all(root_dir())
        .and_then(|_| fs::create_dir_all(sessions_dir()))
        .map_err(|e| app_error(1, format!("Failed to create app dirs: {e}")))
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn session_stamp() -> String {
    Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

fn print_out(cli: &Cli, message: impl AsRef<str>) {
    if !cli.quiet {
        println!("{}", message.as_ref());
    }
}

fn print_verbose(cli: &Cli, message: impl AsRef<str>) {
    if cli.verbose && !cli.quiet {
        eprintln!("[verbose] {}", message.as_ref());
    }
}

fn emit_json(cli: &Cli, payload: &Value) {
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

fn resolve_audio_device(requested: &str, cli: &Cli) -> String {
    if !requested.eq_ignore_ascii_case("auto") {
        return requested.to_string();
    }

    let cache_file = audio_device_cache_file();
    if let Ok(cached) = fs::read_to_string(&cache_file) {
        let cached = cached.trim();
        if cached.starts_with(':') && cached.len() > 1 {
            print_verbose(cli, format!("Using cached audio device {}", cached));
            return cached.to_string();
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

    let started = wait_for_process_start(&mut child, Duration::from_millis(1500))?;
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

fn cmd_start(cli: &Cli, args: &StartArgs) -> Result<i32, AppError> {
    ensure_dirs()?;
    let perf_total = Instant::now();

    let active_path = active_state_file();
    if active_path.exists() {
        let existing: SessionState = read_json(&active_path)?;
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
    let resolved_audio_device = resolve_audio_device(&requested_audio_device, cli);
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

    let record_cmd = build_record_cmd(&audio_path, &resolved_audio_device);
    let t_spawn_recorder = Instant::now();
    let ffmpeg_pid = spawn_recorder(&record_cmd, &ffmpeg_log_path, cli)?;
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
    };

    save_active_state(&state)?;

    let mut prewarm_ms = 0.0;
    // Warm the Parakeet server in the background so stop is faster.
    if parakeet_server_enabled() {
        let t_prewarm = Instant::now();
        if let Some(script_path) = default_parakeet_script() {
            let python_bin = env::var("ISPY_PYTHON_BIN").unwrap_or_else(|_| "python3".to_string());
            let model = env::var("ISPY_PARAKEET_MODEL")
                .unwrap_or_else(|_| "nvidia/parakeet-tdt-0.6b-v2".to_string());
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
        }
    }));

    print_out(
        cli,
        format!(
            "Started session {}\nsession_dir: {}\naudio_path: {}\nscreenshot_source_dir: {}\naudio_device: {}\nstartup_ms: {}",
            session_id,
            session_dir.display(),
            audio_path.display(),
            screenshot_dir.display(),
            resolved_audio_device,
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
            "ffmpeg_pid": ffmpeg_pid,
            "startup_ms": round3(start_total_ms),
            "dry_run": false,
            "state_saved": true
        }),
    );

    Ok(0)
}

fn cmd_shot(cli: &Cli) -> Result<i32, AppError> {
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
            "dry_run": false,
        }),
    );

    Ok(0)
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

fn file_mtime_epoch(path: &Path) -> Option<f64> {
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

fn get_audio_duration_sec(audio_path: &Path) -> Option<f64> {
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

fn inject_screenshot_markers(
    transcript: &str,
    shots: &[ShotMeta],
    audio_duration_sec: Option<f64>,
) -> String {
    let clean = transcript.trim();

    if clean.is_empty() {
        if shots.is_empty() {
            return "_No transcript available._".to_string();
        }
        let mut lines = vec!["_No transcript available._".to_string(), String::new()];
        for shot in shots {
            lines.push(format!("[Screenshot {}]", shot.shot_id));
        }
        return lines.join("\n");
    }

    if shots.is_empty() {
        return clean.to_string();
    }

    let Some(duration) = audio_duration_sec else {
        let tail = shots
            .iter()
            .map(|s| format!("[Screenshot {}]", s.shot_id))
            .collect::<Vec<_>>()
            .join(" ");
        return format!("{}\n\n{}", clean, tail);
    };

    if duration <= 0.0 {
        let tail = shots
            .iter()
            .map(|s| format!("[Screenshot {}]", s.shot_id))
            .collect::<Vec<_>>()
            .join(" ");
        return format!("{}\n\n{}", clean, tail);
    }

    let mut tokens = clean
        .split_whitespace()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    if tokens.is_empty() {
        return clean.to_string();
    }

    let base_len = tokens.len();
    let mut inserted = 0usize;

    for shot in shots {
        let ratio = (shot.audio_sec / duration).clamp(0.0, 1.0);
        let mut idx = ((base_len as f64) * ratio).round() as usize;
        idx = idx.min(tokens.len());
        let marker = format!("[Screenshot {}]", shot.shot_id);
        let insert_at = (idx + inserted).min(tokens.len());
        tokens.insert(insert_at, marker);
        inserted += 1;
    }

    tokens.join(" ")
}

fn format_hms(seconds: f64) -> String {
    let sec = seconds.round().max(0.0) as i64;
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    let s = sec % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn build_note(
    state: &SessionState,
    ended_iso: &str,
    shots: &[ShotMeta],
    transcript: &str,
    transcription_meta: &Value,
    audio_duration_sec: Option<f64>,
) -> String {
    let mut lines = Vec::<String>::new();
    lines.push(format!("# Dictation Session {}", state.session_id));
    lines.push(String::new());
    lines.push(format!("- Started (UTC): {}", state.started_at_iso));
    lines.push(format!("- Ended (UTC): {ended_iso}"));
    lines.push("- Audio: `audio.wav`".to_string());
    lines.push(format!(
        "- Screenshots moved from: `{}`",
        state.screenshot_source_dir
    ));
    lines.push(format!("- Screenshots captured: {}", shots.len()));

    if let Some(duration) = audio_duration_sec {
        lines.push(format!("- Audio duration: {}", format_hms(duration)));
    }

    let t_status = transcription_meta
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    lines.push(format!("- Transcription: `{t_status}`"));

    if let Some(method) = transcription_meta.get("method").and_then(|v| v.as_str()) {
        lines.push(format!("- Transcription method: `{method}`"));
    }

    lines.push(String::new());
    lines.push("## Transcript".to_string());
    lines.push(String::new());

    if !shots.is_empty() {
        let session_dir = Path::new(&state.session_dir);
        for shot in shots {
            let abs_path = session_dir.join(&shot.dest_rel_path);
            lines.push(format!(
                "Screenshot {}: {}",
                shot.shot_id,
                abs_path.display()
            ));
        }
        lines.push(String::new());
    }

    if transcript.trim().is_empty() {
        lines.push("_No transcript available._".to_string());
    } else {
        lines.push(transcript.trim().to_string());
    }
    lines.push(String::new());

    if !shots.is_empty() {
        lines.push("## Screenshot Footnotes".to_string());
        lines.push(String::new());
        for shot in shots {
            lines.push(format!(
                "[Screenshot {}]: {} (t={})",
                shot.shot_id,
                shot.dest_rel_path,
                format_hms(shot.audio_sec)
            ));
        }
        lines.push(String::new());
    }

    lines.push("## Files".to_string());
    lines.push(String::new());
    lines.push("- `audio.wav`".to_string());
    lines.push("- `events.jsonl`".to_string());
    lines.push("- `ffmpeg.log`".to_string());
    lines.push("- `transcript.txt` (if available)".to_string());
    lines.push("- `note.html`".to_string());
    lines.push("- `screenshots/`".to_string());
    lines.push(String::new());

    lines.join("\n")
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn build_html_note(
    session_id: &str,
    started_iso: &str,
    ended_iso: &str,
    audio_duration_sec: Option<f64>,
    transcription_meta: &Value,
    transcript: &str,
    markdown_for_copy: &str,
    shots: &[ShotMeta],
    session_dir: &Path,
) -> String {
    let t_status = transcription_meta
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let t_method = transcription_meta
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("-");

    let mut path_lines = String::new();
    let mut gallery = String::new();
    for shot in shots {
        let abs = session_dir.join(&shot.dest_rel_path);
        let abs_str = abs.display().to_string();
        let url = file_url(&abs);
        path_lines.push_str(&format!("Screenshot {}: {}\n", shot.shot_id, abs_str));
        gallery.push_str(&format!(
            r#"<figure class="card"><div class="card-head"><figcaption>Screenshot {}</figcaption><button class="btn small copy-image" data-url="{}" data-path="{}">Copy image</button></div><a href="{}" target="_blank" rel="noreferrer"><img src="{}" alt="Screenshot {}" loading="lazy" /></a><div class="path">{}</div></figure>"#,
            shot.shot_id,
            html_escape(&url),
            html_escape(&abs_str),
            html_escape(&url),
            html_escape(&url),
            shot.shot_id,
            html_escape(&abs_str)
        ));
    }

    let duration = format_duration_compact(audio_duration_sec);
    let transcript_text = if transcript.trim().is_empty() {
        "_No transcript available._".to_string()
    } else if path_lines.is_empty() {
        transcript.trim().to_string()
    } else {
        format!("{}\n{}", path_lines.trim_end(), transcript.trim())
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Dictation {session_id}</title>
  <style>
    body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; margin: 0; background: #f5f7fb; color: #111827; }}
    .wrap {{ max-width: 1000px; margin: 0 auto; padding: 24px; }}
    h1 {{ margin: 0 0 12px; font-size: 28px; }}
    h2 {{ margin-top: 0; }}
    .meta {{ background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 14px 16px; margin-bottom: 16px; }}
    .meta ul {{ margin: 0; padding-left: 18px; line-height: 1.6; }}
    .panel {{ background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 16px; margin-bottom: 16px; }}
    .actions {{ display: flex; align-items: center; gap: 8px; margin-bottom: 12px; }}
    .status {{ font-size: 12px; color: #475569; }}
    .btn {{ background: #111827; color: #fff; border: 0; border-radius: 8px; padding: 8px 12px; font-size: 13px; cursor: pointer; }}
    .btn:hover {{ background: #1f2937; }}
    .btn.small {{ padding: 6px 10px; font-size: 12px; }}
    .transcript {{ white-space: pre-wrap; line-height: 1.6; font-size: 15px; }}
    .grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(280px, 1fr)); gap: 12px; }}
    .card {{ background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 10px; margin: 0; }}
    .card-head {{ display: flex; justify-content: space-between; align-items: center; gap: 8px; margin-bottom: 8px; }}
    .card img {{ width: 100%; height: auto; border-radius: 8px; display: block; }}
    .card figcaption {{ font-weight: 600; margin: 0; }}
    .path {{ color: #6b7280; margin-top: 8px; font-size: 12px; word-break: break-all; }}
  </style>
</head>
<body>
  <div class="wrap">
    <h1>Dictation Session {session_id}</h1>

    <section class="meta">
      <div class="actions">
        <button id="copyMarkdownBtn" class="btn">Copy markdown</button>
        <button id="copyTranscriptBtn" class="btn">Copy transcript</button>
        <span id="copyStatus" class="status"></span>
      </div>
      <ul>
        <li><strong>Started (UTC):</strong> {started_iso}</li>
        <li><strong>Ended (UTC):</strong> {ended_iso}</li>
        <li><strong>Audio duration:</strong> {duration}</li>
        <li><strong>Screenshots:</strong> {screenshots}</li>
        <li><strong>Transcription status:</strong> {t_status}</li>
        <li><strong>Transcription method:</strong> {t_method}</li>
      </ul>
    </section>

    <section class="panel">
      <h2>Transcript</h2>
      <div class="transcript">{transcript_html}</div>
    </section>

    <section class="panel">
      <h2>Screenshots</h2>
      {gallery_html}
    </section>
  </div>

  <textarea id="markdownContent" style="display:none;">{markdown_html}</textarea>
  <textarea id="transcriptContent" style="display:none;">{transcript_copy_html}</textarea>
  <script>
    const copyStatus = document.getElementById('copyStatus');
    function setStatus(msg) {{
      if (!copyStatus) return;
      copyStatus.textContent = msg;
      window.setTimeout(() => {{
        if (copyStatus.textContent === msg) copyStatus.textContent = '';
      }}, 2000);
    }}

    async function copyText(text, successMessage) {{
      if (!navigator.clipboard || !navigator.clipboard.writeText) {{
        throw new Error('Clipboard text API unavailable');
      }}
      await navigator.clipboard.writeText(text);
      setStatus(successMessage);
    }}

    document.getElementById('copyMarkdownBtn')?.addEventListener('click', async () => {{
      const markdown = document.getElementById('markdownContent')?.value || '';
      try {{
        await copyText(markdown, 'Markdown copied');
      }} catch (err) {{
        setStatus('Could not copy markdown');
      }}
    }});

    document.getElementById('copyTranscriptBtn')?.addEventListener('click', async () => {{
      const transcript = document.getElementById('transcriptContent')?.value || '';
      try {{
        await copyText(transcript, 'Transcript copied');
      }} catch (err) {{
        setStatus('Could not copy transcript');
      }}
    }});

    document.querySelectorAll('.copy-image').forEach((btn) => {{
      btn.addEventListener('click', async () => {{
        const url = btn.dataset.url || '';
        const path = btn.dataset.path || url;

        try {{
          if (!navigator.clipboard || !window.ClipboardItem || !navigator.clipboard.write) {{
            throw new Error('Image clipboard API unavailable');
          }}

          const response = await fetch(url);
          if (!response.ok) throw new Error('Failed to fetch image');
          const blob = await response.blob();
          const type = blob.type || 'image/png';
          await navigator.clipboard.write([new ClipboardItem({{ [type]: blob }})]);
          setStatus('Image copied');
        }} catch (err) {{
          try {{
            await copyText(path, 'Copied image path');
          }} catch (_err) {{
            setStatus('Could not copy image');
          }}
        }}
      }});
    }});
  </script>
</body>
</html>
"#,
        session_id = html_escape(session_id),
        started_iso = html_escape(started_iso),
        ended_iso = html_escape(ended_iso),
        duration = html_escape(&duration),
        screenshots = shots.len(),
        t_status = html_escape(t_status),
        t_method = html_escape(t_method),
        transcript_html = html_escape(&transcript_text),
        transcript_copy_html = html_escape(&transcript_text),
        markdown_html = html_escape(markdown_for_copy),
        gallery_html = if gallery.is_empty() {
            "<div>No screenshots in this session.</div>".to_string()
        } else {
            format!("<div class=\"grid\">{}</div>", gallery)
        },
    )
}

fn shots_from_events(events: &[Value]) -> Vec<ShotMeta> {
    let mut by_id: BTreeMap<usize, ShotMeta> = BTreeMap::new();

    for event in events {
        let etype = event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if etype != "screenshot_moved" && etype != "screenshot_taken" {
            continue;
        }

        let Some(id) = event.get("id").and_then(|v| v.as_u64()).map(|v| v as usize) else {
            continue;
        };
        let Some(dest) = event.get("dest").and_then(|v| v.as_str()) else {
            continue;
        };

        let audio_sec = event
            .get("audioSec")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        by_id.insert(
            id,
            ShotMeta {
                shot_id: id,
                dest_rel_path: dest.to_string(),
                audio_sec,
            },
        );
    }

    by_id.into_values().collect()
}

fn max_shot_id(shots: &[ShotMeta]) -> usize {
    shots.iter().map(|s| s.shot_id).max().unwrap_or(0)
}

fn load_shots_for_session(session_dir: &Path, events: &[Value]) -> Vec<ShotMeta> {
    let mut shots = shots_from_events(events);

    if shots.is_empty() {
        let screenshots_dir = session_dir.join("screenshots");
        if let Ok(entries) = fs::read_dir(screenshots_dir) {
            let mut files = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|s| s.to_ascii_lowercase())
                        .map(|ext| SUPPORTED_IMAGE_EXTS.contains(&ext.as_str()))
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>();
            files.sort();
            shots = files
                .iter()
                .enumerate()
                .map(|(i, p)| ShotMeta {
                    shot_id: i + 1,
                    dest_rel_path: format!(
                        "screenshots/{}",
                        p.file_name().and_then(|n| n.to_str()).unwrap_or_default()
                    ),
                    audio_sec: 0.0,
                })
                .collect();
        }
    }

    shots.sort_by_key(|s| s.shot_id);
    shots
}

fn session_ended_iso(events: &[Value]) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("session_stopped"))
        .and_then(|e| e.get("ts").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn transcription_meta_from_events(events: &[Value]) -> Value {
    events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("session_stopped"))
        .and_then(|e| e.get("transcription").cloned())
        .unwrap_or_else(|| json!({"status": "unknown"}))
}

fn generate_html_for_session(session_dir: &Path) -> Result<PathBuf, AppError> {
    let session_id = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let events = read_jsonl_values(&session_dir.join("events.jsonl"));
    let started_iso = session_started_iso(&events).unwrap_or_else(|| "unknown".to_string());
    let ended_iso = session_ended_iso(&events).unwrap_or_else(|| "unknown".to_string());
    let audio_duration = session_duration_seconds(&events, session_dir);
    let transcription_meta = transcription_meta_from_events(&events);
    let transcript = read_transcript_text_for_session(session_dir);
    let shots = load_shots_for_session(session_dir, &events);

    let note_path = session_dir.join("note.md");
    let markdown_for_copy = if note_path.exists() {
        fs::read_to_string(&note_path).unwrap_or_default()
    } else {
        transcript.clone()
    };

    let html = build_html_note(
        &session_id,
        &started_iso,
        &ended_iso,
        audio_duration,
        &transcription_meta,
        &transcript,
        &markdown_for_copy,
        &shots,
        session_dir,
    );

    let html_path = session_dir.join("note.html");
    fs::write(&html_path, html)
        .map_err(|e| app_error(1, format!("Failed to write {}: {e}", html_path.display())))?;
    Ok(html_path)
}

fn cmd_stop(cli: &Cli, args: &StopArgs) -> Result<i32, AppError> {
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

    if !cli.dry_run {
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
            thread::sleep(Duration::from_millis(400));
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
    let transcript_annotated = inject_screenshot_markers(&transcript_raw, &shots, audio_duration);
    let note_md = build_note(
        &state,
        &ended_iso,
        &shots,
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
        &session_dir,
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
            }),
        )?;

        clear_active_state()?;

        write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
    }

    let stop_total_ms = perf_total.elapsed().as_secs_f64() * 1000.0;
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
            "write_ms": round3(write_ms)
        },
        "transcription_method": transcription_meta.get("method").and_then(|v| v.as_str()),
        "transcription_status": transcription_meta.get("status").and_then(|v| v.as_str())
    }));

    print_out(
        cli,
        format!(
            "Stopped session {}\nsession_dir: {}\nscreenshots_moved: {}\nscreenshots_total: {}\nnote: {}\nhtml: {}\nstop_ms: {}",
            state.session_id,
            session_dir.display(),
            moved_count,
            shots.len(),
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
            "stop_ms": round3(stop_total_ms),
            "phases": {
                "stop_recorder_ms": round3(stop_recorder_ms),
                "move_screenshots_ms": round3(move_screenshots_ms),
                "transcribe_ms": round3(transcribe_ms),
                "render_ms": round3(render_ms),
                "write_ms": round3(write_ms)
            },
            "transcription": transcription_meta,
            "dry_run": cli.dry_run,
        }),
    );

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

fn read_jsonl_values(path: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };

    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

fn session_started_iso(events: &[Value]) -> Option<String> {
    events
        .iter()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("session_started"))
        .and_then(|e| e.get("ts").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn session_duration_seconds(events: &[Value], session_dir: &Path) -> Option<f64> {
    if let Some(duration) = events.iter().rev().find_map(|e| {
        if e.get("type").and_then(|v| v.as_str()) == Some("session_stopped") {
            return e.get("audio_duration_sec").and_then(|v| v.as_f64());
        }
        None
    }) {
        return Some(duration);
    }

    let audio_path = session_dir.join("audio.wav");
    if audio_path.exists() {
        return get_audio_duration_sec(&audio_path);
    }

    None
}

fn count_session_images(session_dir: &Path) -> usize {
    let screenshots_dir = session_dir.join("screenshots");
    let Ok(entries) = fs::read_dir(screenshots_dir) else {
        return 0;
    };

    entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
                .map(|ext| SUPPORTED_IMAGE_EXTS.contains(&ext.as_str()))
                .unwrap_or(false)
        })
        .count()
}

fn extract_transcript_from_note(note_markdown: &str) -> Option<String> {
    let marker = "## Transcript";
    let start = note_markdown.find(marker)? + marker.len();
    let after = note_markdown[start..].trim_start();
    let end = after.find("\n## ").unwrap_or(after.len());
    let section = after[..end].trim();
    if section.is_empty() {
        None
    } else {
        Some(section.to_string())
    }
}

fn read_transcript_text_for_session(session_dir: &Path) -> String {
    let transcript_txt = session_dir.join("transcript.txt");
    if transcript_txt.exists() {
        if let Ok(text) = fs::read_to_string(&transcript_txt) {
            if !text.trim().is_empty() {
                return text;
            }
        }
    }

    let note_md = session_dir.join("note.md");
    if note_md.exists() {
        if let Ok(note) = fs::read_to_string(&note_md) {
            if let Some(section) = extract_transcript_from_note(&note) {
                return section;
            }
        }
    }

    String::new()
}

fn summarize_transcript(text: &str) -> String {
    let normalized = text.trim();
    if normalized.is_empty()
        || normalized.eq_ignore_ascii_case("_No transcript available._")
        || normalized.eq_ignore_ascii_case("No transcript available.")
        || normalized.eq_ignore_ascii_case("No transcript available")
    {
        return "— [0 words]".to_string();
    }

    let cleaned = text
        .lines()
        .filter(|line| {
            let t = line.trim();
            !(t.starts_with("Screenshot ") && t.contains(":"))
        })
        .collect::<Vec<_>>()
        .join(" ");

    let words = cleaned
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '-'))
                .to_string()
        })
        .filter(|w| !w.is_empty())
        .filter(|w| !w.eq_ignore_ascii_case("screenshot"))
        .collect::<Vec<_>>();

    let count = words.len();
    if count == 0 {
        return "— [0 words]".to_string();
    }

    if count <= 6 {
        return format!("{} [{} words]", words.join(" "), count);
    }

    let first = words.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
    let last = words
        .iter()
        .skip(count.saturating_sub(3))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");

    format!("{}..{} [{} words]", first, last, count)
}

fn format_timestamp_human(started_iso: Option<&str>, session_id: &str) -> String {
    let local_dt: Option<DateTime<Local>> = started_iso
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Local))
        .or_else(|| {
            NaiveDateTime::parse_from_str(session_id, "%Y%m%d-%H%M%S")
                .ok()
                .map(|naive| {
                    DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc).with_timezone(&Local)
                })
        });

    let Some(dt) = local_dt else {
        return "unknown".to_string();
    };

    let dow = dt.format("%a").to_string().to_lowercase();
    let (is_pm, hour12) = dt.hour12();
    let ampm = if is_pm { "pm" } else { "am" };
    format!(
        "{} {}-{} {}:{:02}{}",
        dow,
        dt.month(),
        dt.day(),
        hour12,
        dt.minute(),
        ampm
    )
}

fn format_duration_compact(seconds: Option<f64>) -> String {
    let Some(raw) = seconds else {
        return "-".to_string();
    };
    let sec = raw.round().max(0.0) as i64;

    if sec < 60 {
        return format!("{}s", sec);
    }
    if sec < 3600 {
        let m = sec / 60;
        let s = sec % 60;
        if s == 0 {
            return format!("{}m", m);
        }
        return format!("{}m {}s", m, s);
    }

    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    if m == 0 {
        format!("{}h", h)
    } else {
        format!("{}h {}m", h, m)
    }
}

fn truncate_to_width(input: &str, max_width: usize) -> String {
    let chars = input.chars().collect::<Vec<_>>();
    if chars.len() <= max_width {
        return input.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    chars[..max_width - 1].iter().collect::<String>() + "…"
}

fn sep_line(widths: &[usize]) -> String {
    let mut line = String::new();
    line.push('+');
    for width in widths {
        line.push_str(&"-".repeat(*width + 2));
        line.push('+');
    }
    line
}

fn render_sessions_table(rows: &[SessionListRow]) -> String {
    let session_header = "session";
    let time_header = "timestamp";
    let summary_header = "summary";
    let images_header = "imgs";
    let length_header = "length";

    let session_w = std::cmp::max(
        session_header.len(),
        rows.iter().map(|r| r.session_id.len()).max().unwrap_or(0),
    );
    let time_w = std::cmp::max(
        time_header.len(),
        rows.iter().map(|r| r.timestamp.len()).max().unwrap_or(0),
    );
    let images_w = std::cmp::max(
        images_header.len(),
        rows.iter()
            .map(|r| r.images.to_string().len())
            .max()
            .unwrap_or(1),
    );
    let length_w = std::cmp::max(
        length_header.len(),
        rows.iter().map(|r| r.duration.len()).max().unwrap_or(1),
    );

    let summary_raw_max = std::cmp::max(
        summary_header.len(),
        rows.iter()
            .map(|r| r.summary.chars().count())
            .max()
            .unwrap_or(0),
    );

    let term_cols = env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(140);

    let fixed_without_summary = session_w + time_w + images_w + length_w + 16;
    let summary_w = if term_cols > fixed_without_summary + 20 {
        let available = term_cols - fixed_without_summary;
        std::cmp::max(20, std::cmp::min(summary_raw_max, available))
    } else {
        std::cmp::max(20, std::cmp::min(summary_raw_max, 70))
    };

    let widths = [session_w, time_w, summary_w, images_w, length_w];
    let mut lines = Vec::new();

    let sep = sep_line(&widths);
    lines.push(sep.clone());
    lines.push(format!(
        "| {:<session_w$} | {:<time_w$} | {:<summary_w$} | {:>images_w$} | {:>length_w$} |",
        session_header,
        time_header,
        summary_header,
        images_header,
        length_header,
        session_w = session_w,
        time_w = time_w,
        summary_w = summary_w,
        images_w = images_w,
        length_w = length_w,
    ));
    lines.push(sep.clone());

    for row in rows {
        lines.push(format!(
            "| {:<session_w$} | {:<time_w$} | {:<summary_w$} | {:>images_w$} | {:>length_w$} |",
            row.session_id,
            row.timestamp,
            truncate_to_width(&row.summary, summary_w),
            row.images,
            row.duration,
            session_w = session_w,
            time_w = time_w,
            summary_w = summary_w,
            images_w = images_w,
            length_w = length_w,
        ));
    }

    lines.push(sep);
    lines.join("\n")
}

fn collect_recent_session_dirs(limit: usize) -> Result<Vec<PathBuf>, AppError> {
    let mut dirs = fs::read_dir(sessions_dir())
        .map_err(|e| app_error(1, format!("Failed to read sessions dir: {e}")))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect::<Vec<_>>();

    dirs.sort_by(|a, b| {
        b.file_name()
            .unwrap_or_default()
            .cmp(a.file_name().unwrap_or_default())
    });

    if dirs.len() > limit {
        dirs.truncate(limit);
    }
    Ok(dirs)
}

fn build_list_row(session_dir: &Path) -> SessionListRow {
    let session_id = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let events = read_jsonl_values(&session_dir.join("events.jsonl"));
    let started_iso = session_started_iso(&events);
    let timestamp = format_timestamp_human(started_iso.as_deref(), &session_id);

    let transcript_text = read_transcript_text_for_session(session_dir);
    let summary = summarize_transcript(&transcript_text);

    let images = count_session_images(session_dir);
    let duration = format_duration_compact(session_duration_seconds(&events, session_dir));

    SessionListRow {
        session_id,
        timestamp,
        summary,
        images,
        duration,
    }
}

fn resolve_recent_session_dir(rank: usize) -> Result<PathBuf, AppError> {
    if rank == 0 {
        return Err(app_error(8, "Session index must be >= 1."));
    }

    let session_dirs = collect_recent_session_dirs(rank)?;
    if session_dirs.is_empty() {
        return Err(app_error(8, "No sessions found."));
    }
    if session_dirs.len() < rank {
        return Err(app_error(
            8,
            format!(
                "Requested session {} but only {} session(s) exist.",
                rank,
                session_dirs.len()
            ),
        ));
    }

    Ok(session_dirs[rank - 1].clone())
}

fn cmd_list(cli: &Cli, args: &ListArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let requested = args.n.unwrap_or(10);
    let limit = requested.clamp(1, 200);
    let session_dirs = collect_recent_session_dirs(limit)?;
    if session_dirs.is_empty() {
        print_out(cli, "No sessions found.");
        emit_json(
            cli,
            &json!({
                "ok": true,
                "count": 0,
                "sessions": []
            }),
        );
        return Ok(0);
    }

    let rows = session_dirs
        .iter()
        .map(|dir| build_list_row(dir))
        .collect::<Vec<_>>();

    let table = render_sessions_table(&rows);
    print_out(cli, table);

    emit_json(
        cli,
        &json!({
            "ok": true,
            "count": rows.len(),
            "sessions": rows,
        }),
    );

    Ok(0)
}

fn cmd_copy(_cli: &Cli, args: &CopyArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let requested_rank = args.n.unwrap_or(1);
    let session_dir = resolve_recent_session_dir(requested_rank)?;
    let note_path = session_dir.join("note.md");

    let transcript = if note_path.exists() {
        let markdown = fs::read_to_string(&note_path)
            .map_err(|e| app_error(1, format!("Failed to read {}: {e}", note_path.display())))?;
        extract_transcript_from_note(&markdown).unwrap_or_default()
    } else {
        String::new()
    };

    let transcript = if transcript.trim().is_empty() {
        let transcript_txt_path = session_dir.join("transcript.txt");
        if transcript_txt_path.exists() {
            fs::read_to_string(&transcript_txt_path).map_err(|e| {
                app_error(
                    1,
                    format!("Failed to read {}: {e}", transcript_txt_path.display()),
                )
            })?
        } else {
            String::new()
        }
    } else {
        transcript
    };

    if transcript.trim().is_empty() {
        return Err(app_error(
            8,
            format!("No transcript found for session: {}", session_dir.display()),
        ));
    }

    // Intentionally raw stdout only, so this can be piped/copied easily.
    println!("{}", transcript.trim());
    Ok(0)
}

fn cmd_show(_cli: &Cli, args: &ShowArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let requested_rank = args.n.unwrap_or(1);
    let session_dir = resolve_recent_session_dir(requested_rank)?;
    let note_path = session_dir.join("note.md");

    if !note_path.exists() {
        return Err(app_error(
            8,
            format!("No note.md found for session: {}", session_dir.display()),
        ));
    }

    let markdown = fs::read_to_string(&note_path)
        .map_err(|e| app_error(1, format!("Failed to read {}: {e}", note_path.display())))?;

    // Intentionally raw stdout only, so this can be piped or viewed directly.
    print!("{}", markdown);
    Ok(0)
}

fn cmd_html(cli: &Cli, args: &HtmlArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let requested_rank = args.n.unwrap_or(1);
    let session_dir = resolve_recent_session_dir(requested_rank)?;

    // Always regenerate so HTML reflects latest template/features.
    let html_path = generate_html_for_session(&session_dir)?;

    // Print path first so it is easy to capture in scripts.
    println!("{}", html_path.display());
    if !cli.quiet {
        println!("Opening {}", html_path.display());
    }

    let status = Command::new("open")
        .arg(OsString::from(&html_path))
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
        }),
    );

    Ok(0)
}

fn cmd_last(cli: &Cli, args: &LastArgs) -> Result<i32, AppError> {
    ensure_dirs()?;
    let path = last_session_file();
    if !path.exists() {
        return Err(app_error(8, "No previous session found."));
    }

    let data: Value = read_json(&path)?;
    let session_id = data
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let session_dir = data
        .get("session_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let note_path = data
        .get("note_path")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    print_out(
        cli,
        format!(
            "Last session: {}\nsession_dir: {}\nnote: {}",
            session_id, session_dir, note_path
        ),
    );

    emit_json(
        cli,
        &json!({
            "ok": true,
            "session_id": session_id,
            "session_dir": session_dir,
            "note_path": note_path,
        }),
    );

    if args.open {
        let status = Command::new("open")
            .arg(OsString::from(note_path))
            .status()
            .map_err(|e| app_error(1, format!("Failed to run 'open': {e}")))?;
        if !status.success() {
            return Err(app_error(
                1,
                format!("open command failed with status: {status}"),
            ));
        }
    }

    Ok(0)
}

fn run(cli: &Cli) -> Result<i32, AppError> {
    match &cli.command {
        Commands::Start(args) => cmd_start(cli, args),
        Commands::Shot => cmd_shot(cli),
        Commands::Stop(args) => cmd_stop(cli, args),
        Commands::Status => cmd_status(cli),
        Commands::Last(args) => cmd_last(cli, args),
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
