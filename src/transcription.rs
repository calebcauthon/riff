use crate::cli::{Cli, StopArgs};
use crate::error::{app_error, AppError};
use crate::models::SessionState;
use crate::paths::{
    parakeet_server_log_file, parakeet_server_pid_file, root_dir, web_server_log_file,
    web_server_pid_file,
};
use crate::setup::default_user_runtime_dir;
use crate::{
    command_exists, fill_template, fill_template_with_transcript, print_verbose, process_is_alive,
    read_pid_file, round3, shell_escape, write_pid_file,
};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_PARAKEET_MODEL: &str = "nvidia/stt_en_fastconformer_hybrid_medium_streaming_80ms_pc";
const DEFAULT_PARAKEET_SERVER_WAIT_READY_SEC: u64 = 30;
const DEFAULT_PARAKEET_BATCH_SIZE: u32 = 4;

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn perf_mark(perf: &mut Map<String, Value>, key: &str, start: Instant) {
    perf.insert(key.to_string(), json!(round3(elapsed_ms(start))));
}

fn attach_perf(mut meta: Value, mut perf: Map<String, Value>, started: Instant) -> Value {
    perf.insert("total_ms".to_string(), json!(round3(elapsed_ms(started))));
    if let Some(obj) = meta.as_object_mut() {
        obj.insert("perf".to_string(), Value::Object(perf));
    }
    meta
}

fn executable_ancestor_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = env::current_exe() {
        let mut parent = exe.parent();
        for _ in 0..7 {
            let Some(p) = parent else {
                break;
            };
            roots.push(p.to_path_buf());
            parent = p.parent();
        }
    }

    let mut seen: HashSet<PathBuf> = HashSet::new();
    roots
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

pub(crate) fn resource_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("RIFF_RESOURCE_DIR").map(PathBuf::from) {
        if dir.exists() {
            return Some(dir);
        }
    }

    for root in executable_ancestor_roots() {
        for candidate in [&root, &root.join("libexec")] {
            if candidate.join("scripts").exists() {
                return Some(candidate.to_path_buf());
            }
        }
    }

    None
}

fn first_existing_relative(paths: &[&str]) -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = resource_dir() {
        roots.push(dir);
    }
    roots.extend(executable_ancestor_roots());

    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in roots.into_iter().filter(|p| seen.insert(p.clone())) {
        for rel in paths {
            let candidate = root.join(rel);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn default_local_python_bin() -> Option<PathBuf> {
    let setup_runtime = default_user_runtime_dir();
    for rel in ["bin/python3", "bin/python"] {
        let candidate = setup_runtime.join(rel);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    first_existing_relative(&[
        "runtime/python/bin/python3",
        "runtime/python/bin/python",
        ".venv/bin/python3",
        ".venv/bin/python",
    ])
}

pub(crate) fn resolve_python_bin(explicit: Option<&str>) -> String {
    if let Some(bin) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return bin.to_string();
    }

    if let Some(bin) = env::var("RIFF_PYTHON_BIN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return bin;
    }

    if let Some(path) = default_local_python_bin() {
        return path.display().to_string();
    }

    "python3".to_string()
}

pub(crate) fn resolve_parakeet_model(explicit: Option<&str>) -> String {
    if let Some(model) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return model.to_string();
    }

    if let Some(model) = env::var("RIFF_PARAKEET_MODEL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return model;
    }

    DEFAULT_PARAKEET_MODEL.to_string()
}

pub(crate) fn resolve_parakeet_batch_size() -> u32 {
    env::var("RIFF_PARAKEET_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_PARAKEET_BATCH_SIZE)
}

pub(crate) fn resolve_parakeet_script(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }

    if let Some(path) = env::var("RIFF_PARAKEET_SCRIPT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    {
        return Some(path);
    }

    default_parakeet_script()
}

pub(crate) fn default_parakeet_script() -> Option<PathBuf> {
    first_existing_relative(&["scripts/parakeet_transcribe.py"])
}

pub(crate) fn parakeet_server_enabled() -> bool {
    env::var("RIFF_PARAKEET_SERVER")
        .map(|v| {
            !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true)
}

pub(crate) fn parakeet_server_base_url() -> String {
    env::var("RIFF_PARAKEET_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:8765".to_string())
}

fn parakeet_server_wait_ready_timeout_sec() -> u64 {
    env::var("RIFF_PARAKEET_SERVER_WAIT_READY_SEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_PARAKEET_SERVER_WAIT_READY_SEC)
}

fn parakeet_server_health_url(base: &str) -> String {
    format!("{}/health", base.trim_end_matches('/'))
}

fn parakeet_server_transcribe_url(base: &str) -> String {
    format!("{}/transcribe", base.trim_end_matches('/'))
}

pub(crate) fn check_parakeet_server_health(base_url: &str) -> bool {
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
) -> Result<i32, AppError> {
    let pid_file = parakeet_server_pid_file();
    if let Some(pid) = read_pid_file(&pid_file) {
        if process_is_alive(pid) {
            print_verbose(
                cli,
                format!("Parakeet server process already running (pid={})", pid),
            );
            return Ok(pid);
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
        .arg(model)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
        .map_err(|e| app_error(1, format!("Failed to start Parakeet server: {e}")))?;

    let pid = child.id() as i32;
    write_pid_file(&pid_file, pid);
    Ok(pid)
}

pub(crate) fn ensure_parakeet_server(
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

    let spawned_pid = match spawn_parakeet_server(python_bin, script_path, model, cli) {
        Ok(pid) => Some(pid),
        Err(e) => {
            print_verbose(
                cli,
                format!("Failed to start Parakeet server: {}", e.message),
            );
            None
        }
    };
    if !wait_ready {
        return;
    }

    let deadline = Instant::now() + Duration::from_secs(parakeet_server_wait_ready_timeout_sec());
    let mut saw_dead_server_process = false;
    while Instant::now() < deadline {
        if check_parakeet_server_health(&base_url) {
            print_verbose(cli, format!("Parakeet server ready at {}", base_url));
            return;
        }
        if let Some(pid) = spawned_pid {
            if !process_is_alive(pid) {
                saw_dead_server_process = true;
                print_verbose(
                    cli,
                    format!(
                        "Parakeet server process exited before becoming healthy (pid={pid}); see {}",
                        parakeet_server_log_file().display()
                    ),
                );
                break;
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    print_verbose(
        cli,
        format!(
            "Parakeet server not ready yet at {} (will fallback, wait_timeout_sec={}, server_process_exited={})",
            base_url,
            parakeet_server_wait_ready_timeout_sec(),
            saw_dead_server_process
        ),
    );
}

fn web_server_enabled() -> bool {
    env::var("RIFF_WEB_SERVER")
        .map(|v| {
            !matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true)
}

pub(crate) fn web_server_base_url() -> String {
    env::var("RIFF_WEB_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:8766".to_string())
}

fn web_server_idle_timeout_sec() -> u64 {
    env::var("RIFF_WEB_SERVER_IDLE_TIMEOUT_SEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1800)
}

pub(crate) fn default_web_server_script() -> Option<PathBuf> {
    first_existing_relative(&["scripts/riff_web_server.py"])
}

pub(crate) fn default_sound_picker_script() -> Option<PathBuf> {
    first_existing_relative(&["scripts/pick_riff_sounds.sh"])
}

fn web_server_health_url(base: &str) -> String {
    format!("{}/health", base.trim_end_matches('/'))
}

fn web_server_touch_url(base: &str) -> String {
    format!("{}/touch", base.trim_end_matches('/'))
}

pub(crate) fn check_web_server_health(base_url: &str) -> bool {
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

pub(crate) fn touch_web_server(base_url: &str) -> bool {
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

/// Parse `RIFF_WEB_SERVER_URL` (or the default) into a validated `(host, port)`
/// pair, ensuring the server only ever gets started against a loopback
/// address. This is a structural parse (scheme/host/port) rather than a
/// naive `split(':')`, so it correctly rejects things like IPv6 literals
/// (`[::1]:8766`), paths/queries, and non-loopback hosts instead of silently
/// misinterpreting them.
fn parse_web_server_loopback_url(raw: &str) -> Result<(String, String), AppError> {
    let invalid = |detail: &str| {
        app_error(
            1,
            format!(
                "Invalid RIFF_WEB_SERVER_URL {:?}: {}. Expected an http URL with a loopback host, e.g. http://127.0.0.1:8766",
                raw, detail
            ),
        )
    };

    let rest = raw
        .strip_prefix("http://")
        .ok_or_else(|| invalid("only the http scheme is supported"))?;

    // Disallow userinfo (user:pass@host), path, query, or fragment components;
    // we only expect "host[:port]".
    if rest.contains('@') || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err(invalid(
            "only a bare host[:port] is supported (no path, query, or credentials)",
        ));
    }

    // Structural split into host and optional port. IPv6 literals are
    // bracketed, e.g. "[::1]:8766" or bare "[::1]".
    let (host, port) = if let Some(after_bracket) = rest.strip_prefix('[') {
        let (host, remainder) = after_bracket
            .split_once(']')
            .ok_or_else(|| invalid("unterminated IPv6 literal (missing ']')"))?;
        let port = if let Some(p) = remainder.strip_prefix(':') {
            p
        } else if remainder.is_empty() {
            "8766"
        } else {
            return Err(invalid("unexpected characters after IPv6 literal"));
        };
        (host.to_string(), port.to_string())
    } else {
        match rest.split_once(':') {
            Some((host, port)) => (host.to_string(), port.to_string()),
            None => (rest.to_string(), "8766".to_string()),
        }
    };

    if host.is_empty() {
        return Err(invalid("host is empty"));
    }

    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || (host.starts_with("127.") && host.split('.').count() == 4 && host.parse::<std::net::Ipv4Addr>().is_ok());

    if !is_loopback {
        return Err(invalid(
            "host must be a loopback address (127.0.0.1, localhost, or ::1)",
        ));
    }

    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) || port.parse::<u16>().is_err()
    {
        return Err(invalid("port must be a number between 0 and 65535"));
    }

    Ok((host, port))
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
    let (host, port) = parse_web_server_loopback_url(&base_url)?;

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

    let mut cmd = Command::new(python_bin);
    cmd.arg(script_path)
        .arg("--root")
        .arg(root_dir())
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port)
        .arg("--idle-timeout-sec")
        .arg(web_server_idle_timeout_sec().to_string())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err));

    if let Ok(exe) = env::current_exe() {
        cmd.env("RIFF_BIN", &exe);
    }

    let child = cmd
        .spawn()
        .map_err(|e| app_error(1, format!("Failed to start web server: {e}")))?;

    write_pid_file(&pid_file, child.id() as i32);
    Ok(())
}

pub(crate) fn ensure_web_server(cli: &Cli, wait_ready: bool) -> bool {
    if !web_server_enabled() {
        return false;
    }

    let base_url = web_server_base_url();
    if check_web_server_health(&base_url) {
        return true;
    }

    let python_bin = resolve_python_bin(None);
    let Some(script_path) = env::var("RIFF_WEB_SERVER_SCRIPT")
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

    if let Err(e) = spawn_web_server(&python_bin, &script_path, cli) {
        print_verbose(cli, format!("Skipping web server startup: {}", e.message));
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
    batch_size: u32,
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
        "batch_size": batch_size
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
            "batch_size": batch_size,
            "elapsed_sec": parsed.get("elapsed_sec").and_then(|v| v.as_f64()),
        }),
    ))
}

pub(crate) fn run_transcription(
    state: &SessionState,
    session_dir: &Path,
    stop_args: &StopArgs,
    cli: &Cli,
) -> (String, Value) {
    let perf_total = Instant::now();
    let mut perf: Map<String, Value> = Map::new();

    if cli.dry_run {
        perf.insert("execution_path".to_string(), json!("dry_run"));
        return (
            String::new(),
            attach_perf(
                json!({"status": "dry_run", "reason": "transcription skipped"}),
                perf,
                perf_total,
            ),
        );
    }

    let t_audio_exists = Instant::now();
    let audio_path = PathBuf::from(&state.audio_path);
    perf_mark(&mut perf, "audio_exists_check_ms", t_audio_exists);
    if !audio_path.exists() {
        perf.insert("execution_path".to_string(), json!("missing_audio"));
        return (
            String::new(),
            attach_perf(
                json!({
                    "status": "missing_audio",
                    "reason": format!("Audio file not found: {}", audio_path.display())
                }),
                perf,
                perf_total,
            ),
        );
    }

    let out_base = session_dir.join("transcript");
    let out_txt = session_dir.join("transcript.txt");

    let cmd_template = stop_args
        .transcribe_cmd
        .clone()
        .or_else(|| env::var("RIFF_TRANSCRIBE_CMD").ok())
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(template) = cmd_template {
        perf.insert("execution_path".to_string(), json!("custom_command"));
        let filled = fill_template(&template, &audio_path, &out_base, &out_txt, session_dir);
        print_verbose(cli, format!("Running transcription command: {filled}"));

        let t_custom_cmd = Instant::now();
        let output = Command::new("sh").arg("-lc").arg(&filled).output();
        perf_mark(&mut perf, "custom_command_ms", t_custom_cmd);
        match output {
            Ok(out) if out.status.success() => {
                let t_read_output = Instant::now();
                let txt = if out_txt.exists() {
                    fs::read_to_string(&out_txt).unwrap_or_default()
                } else {
                    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                    if !stdout.trim().is_empty() {
                        let _ = fs::write(&out_txt, stdout.as_bytes());
                    }
                    stdout
                };
                perf_mark(&mut perf, "custom_output_read_ms", t_read_output);
                return (
                    txt.trim().to_string(),
                    attach_perf(
                        json!({"status": "ok", "method": "custom_command", "cmd": filled}),
                        perf,
                        perf_total,
                    ),
                );
            }
            Ok(out) => {
                return (
                    String::new(),
                    attach_perf(
                        json!({
                            "status": "error",
                            "method": "custom_command",
                            "cmd": filled,
                            "returncode": out.status.code(),
                            "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string()
                        }),
                        perf,
                        perf_total,
                    ),
                );
            }
            Err(e) => {
                return (
                    String::new(),
                    attach_perf(
                        json!({
                            "status": "error",
                            "method": "custom_command",
                            "cmd": filled,
                            "reason": format!("Failed to spawn shell command: {e}")
                        }),
                        perf,
                        perf_total,
                    ),
                );
            }
        }
    }

    perf.insert("execution_path".to_string(), json!("parakeet"));
    let script = resolve_parakeet_script(stop_args.parakeet_script.as_deref());

    let Some(script_path) = script else {
        return (
            String::new(),
            attach_perf(
                json!({
                    "status": "skipped",
                    "reason": "No transcription configured. Set --parakeet-script or RIFF_PARAKEET_SCRIPT, or use --transcribe-cmd."
                }),
                perf,
                perf_total,
            ),
        );
    };

    let python_bin = resolve_python_bin(stop_args.python_bin.as_deref());
    let model = resolve_parakeet_model(stop_args.parakeet_model.as_deref());
    let batch_size = resolve_parakeet_batch_size();
    perf.insert("python_bin".to_string(), json!(python_bin.clone()));
    perf.insert("model".to_string(), json!(model.clone()));
    perf.insert("batch_size".to_string(), json!(batch_size));
    perf.insert(
        "script_path".to_string(),
        json!(script_path.display().to_string()),
    );

    let mut server_error: Option<Value> = None;

    let server_enabled = parakeet_server_enabled();
    perf.insert("server_enabled".to_string(), json!(server_enabled));
    if server_enabled {
        let base_url = parakeet_server_base_url();
        perf.insert("server_url".to_string(), json!(base_url.clone()));

        let t_server_health_before = Instant::now();
        let server_health_before = check_parakeet_server_health(&base_url);
        perf_mark(
            &mut perf,
            "server_health_before_check_ms",
            t_server_health_before,
        );
        perf.insert(
            "server_health_before".to_string(),
            json!(server_health_before),
        );

        let t_server_ensure = Instant::now();
        ensure_parakeet_server(&python_bin, &script_path, &model, cli, true);
        perf_mark(&mut perf, "server_ensure_ms", t_server_ensure);

        let t_server_health_after = Instant::now();
        let server_health_after = check_parakeet_server_health(&base_url);
        perf_mark(
            &mut perf,
            "server_health_after_check_ms",
            t_server_health_after,
        );
        perf.insert(
            "server_health_after".to_string(),
            json!(server_health_after),
        );

        if let Some(pid) = read_pid_file(&parakeet_server_pid_file()) {
            perf.insert("server_pid".to_string(), json!(pid));
            perf.insert("server_pid_alive".to_string(), json!(process_is_alive(pid)));
        }

        if server_health_after {
            let t_server_request = Instant::now();
            match transcribe_via_parakeet_server(
                &base_url,
                &audio_path,
                &out_txt,
                &model,
                batch_size,
            ) {
                Ok((txt, meta)) => {
                    perf_mark(&mut perf, "server_request_ms", t_server_request);
                    if !txt.is_empty() {
                        let t_write_txt = Instant::now();
                        let _ = fs::write(&out_txt, format!("{}\n", txt));
                        perf_mark(&mut perf, "server_write_transcript_ms", t_write_txt);
                    }
                    return (txt, attach_perf(meta, perf, perf_total));
                }
                Err(meta) => {
                    perf_mark(&mut perf, "server_request_ms", t_server_request);
                    server_error = Some(meta.clone());
                    print_verbose(
                        cli,
                        format!(
                            "Parakeet server transcription failed, falling back to one-shot process: {}",
                            meta
                        ),
                    );
                }
            }
        } else {
            print_verbose(
                cli,
                format!(
                    "Parakeet server unavailable at {} after ensure; falling back to one-shot process.",
                    base_url
                ),
            );
        }
    }

    let cmd_for_log = format!(
        "{} {} --audio {} --out-txt {} --model {} --batch-size {}",
        shell_escape(&python_bin),
        shell_escape(&script_path.display().to_string()),
        shell_escape(&audio_path.display().to_string()),
        shell_escape(&out_txt.display().to_string()),
        shell_escape(&model),
        batch_size
    );

    print_verbose(
        cli,
        format!("Running Parakeet transcription (one-shot): {cmd_for_log}"),
    );

    let t_python = Instant::now();
    let output = Command::new(&python_bin)
        .arg(&script_path)
        .arg("--audio")
        .arg(&audio_path)
        .arg("--out-txt")
        .arg(&out_txt)
        .arg("--model")
        .arg(&model)
        .arg("--batch-size")
        .arg(batch_size.to_string())
        .output();
    perf_mark(&mut perf, "python_transcribe_ms", t_python);

    match output {
        Ok(out) if out.status.success() => {
            let t_read_output = Instant::now();
            let txt = if out_txt.exists() {
                fs::read_to_string(&out_txt).unwrap_or_default()
            } else {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                if !stdout.trim().is_empty() {
                    let _ = fs::write(&out_txt, stdout.as_bytes());
                }
                stdout
            };
            perf_mark(&mut perf, "python_output_read_ms", t_read_output);

            let mut meta = json!({
                "status": "ok",
                "method": "parakeet_python",
                "cmd": cmd_for_log,
                "script": script_path,
                "model": model,
                "batch_size": batch_size,
            });
            if let Some(server_error_meta) = server_error {
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert("fallback_from".to_string(), json!("parakeet_server"));
                    obj.insert("server_error".to_string(), server_error_meta);
                }
            }

            (txt.trim().to_string(), attach_perf(meta, perf, perf_total))
        }
        Ok(out) => {
            let mut meta = json!({
                "status": "error",
                "method": "parakeet_python",
                "cmd": cmd_for_log,
                "returncode": out.status.code(),
                "signal": out.status.signal(),
                "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string(),
                "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(),
            });
            if let Some(server_error_meta) = server_error {
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert("fallback_from".to_string(), json!("parakeet_server"));
                    obj.insert("server_error".to_string(), server_error_meta);
                }
            }
            (String::new(), attach_perf(meta, perf, perf_total))
        }
        Err(e) => {
            let mut meta = json!({
                "status": "error",
                "method": "parakeet_python",
                "cmd": cmd_for_log,
                "reason": format!("Failed to run python transcription: {e}")
            });
            if let Some(server_error_meta) = server_error {
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert("fallback_from".to_string(), json!("parakeet_server"));
                    obj.insert("server_error".to_string(), server_error_meta);
                }
            }
            (String::new(), attach_perf(meta, perf, perf_total))
        }
    }
}

pub(crate) fn run_post_transcribe_command(
    transcript: &str,
    state: &SessionState,
    session_dir: &Path,
    stop_args: &StopArgs,
    cli: &Cli,
) -> (String, Value) {
    if stop_args.no_stop_hooks {
        return (
            transcript.to_string(),
            json!({"status": "skipped", "reason": "disabled_by_flag"}),
        );
    }

    let cmd_template = stop_args
        .post_transcribe_cmd
        .clone()
        .or_else(|| env::var("RIFF_POST_TRANSCRIBE_CMD").ok())
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty());

    let Some(template) = cmd_template else {
        return (
            transcript.to_string(),
            json!({"status": "skipped", "reason": "not_configured"}),
        );
    };

    let audio_path = PathBuf::from(&state.audio_path);
    let out_base = session_dir.join("transcript");
    let out_txt = session_dir.join("transcript.txt");
    let filled = fill_template_with_transcript(
        &template,
        &audio_path,
        &out_base,
        &out_txt,
        session_dir,
        Some(transcript),
    );
    print_verbose(cli, format!("Running post-transcribe command: {filled}"));

    match Command::new("sh").arg("-lc").arg(&filled).output() {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let rewritten = if !stdout.trim().is_empty() {
                stdout
            } else if out_txt.exists() {
                fs::read_to_string(&out_txt).unwrap_or_default()
            } else {
                String::new()
            };
            let rewritten_trimmed = rewritten.trim().to_string();
            let _ = fs::write(
                &out_txt,
                if rewritten_trimmed.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", rewritten_trimmed)
                },
            );
            (
                rewritten_trimmed,
                json!({
                    "status": "ok",
                    "method": "custom_command",
                    "cmd": filled,
                }),
            )
        }
        Ok(out) => (
            transcript.to_string(),
            json!({
                "status": "error",
                "method": "custom_command",
                "cmd": filled,
                "returncode": out.status.code(),
                "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }),
        ),
        Err(e) => (
            transcript.to_string(),
            json!({
                "status": "error",
                "method": "custom_command",
                "cmd": filled,
                "reason": format!("Failed to spawn shell command: {e}"),
            }),
        ),
    }
}

/// Run the configured `RIFF_HOOKS` chain against the transcript.
///
/// Each hook is a bash command. Hooks run in order and are invoked as
/// `sh -lc '<hook>' riff-hook <transcript_path> <metadata_path>` so that:
///   - `$1` is a temp file containing the current transcript (edit in place)
///   - `$2` is a temp file containing a read-only JSON blob of session metadata
///
/// After each hook runs, the transcript temp file is read back and becomes the
/// input for the next hook. The final transcript is written to
/// `<session_dir>/transcript.txt`.
/// Normalize an ad-hoc `--with-post-hook` value into a shell command for the
/// hook runner. A bare script path (no shell variable reference) gets `"$@"`
/// appended so the transcript (`$1`) and metadata (`$2`) temp files are
/// forwarded to it; a value that already references `$1`/`$2`/`$@` is used as-is.
fn normalize_cli_hook(raw: &str) -> String {
    let hook = raw.trim();
    if hook.contains('$') {
        hook.to_string()
    } else {
        format!(r#"{hook} "$@""#)
    }
}

pub(crate) fn run_output_hooks(
    transcript: &str,
    metadata: &Value,
    session_dir: &Path,
    stop_args: &StopArgs,
    cli: &Cli,
) -> (String, Value) {
    if stop_args.no_stop_hooks {
        return (
            transcript.to_string(),
            json!({"status": "skipped", "reason": "disabled_by_flag"}),
        );
    }

    // The configured RIFF_HOOKS chain runs first (unless --no-hooks), followed
    // by any ad-hoc --with-post-hook hooks passed on the command line.
    let mut hooks: Vec<String> = if stop_args.no_hooks {
        Vec::new()
    } else {
        env::var("RIFF_HOOKS")
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    };
    hooks.extend(
        stop_args
            .with_post_hook
            .iter()
            .map(|h| h.trim())
            .filter(|h| !h.is_empty())
            .map(normalize_cli_hook),
    );

    if hooks.is_empty() {
        let reason = if stop_args.no_hooks {
            "disabled_by_flag"
        } else {
            "not_configured"
        };
        return (
            transcript.to_string(),
            json!({"status": "skipped", "reason": reason}),
        );
    }

    let unique = format!(
        "riff-hooks-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp_dir = env::temp_dir().join(unique);
    if let Err(e) = fs::create_dir_all(&tmp_dir) {
        return (
            transcript.to_string(),
            json!({"status": "error", "reason": format!("Failed to create hook temp dir: {e}")}),
        );
    }
    let transcript_path = tmp_dir.join("transcript.txt");
    let metadata_path = tmp_dir.join("metadata.json");

    if let Err(e) = fs::write(&transcript_path, transcript) {
        let _ = fs::remove_dir_all(&tmp_dir);
        return (
            transcript.to_string(),
            json!({"status": "error", "reason": format!("Failed to write transcript temp file: {e}")}),
        );
    }
    let metadata_text = serde_json::to_string_pretty(metadata).unwrap_or_else(|_| "{}".to_string());
    if let Err(e) = fs::write(&metadata_path, metadata_text) {
        let _ = fs::remove_dir_all(&tmp_dir);
        return (
            transcript.to_string(),
            json!({"status": "error", "reason": format!("Failed to write metadata temp file: {e}")}),
        );
    }

    let mut current = transcript.to_string();
    let mut results: Vec<Value> = Vec::new();
    let mut overall_error = false;

    for (idx, hook) in hooks.iter().enumerate() {
        print_verbose(cli, format!("Running output hook {}: {hook}", idx + 1));
        let output = Command::new("sh")
            .arg("-lc")
            .arg(hook)
            .arg("riff-hook")
            .arg(&transcript_path)
            .arg(&metadata_path)
            .output();
        match output {
            Ok(out) if out.status.success() => {
                current = fs::read_to_string(&transcript_path).unwrap_or(current);
                results.push(json!({
                    "hook": hook,
                    "status": "ok",
                    "chars": current.chars().count(),
                }));
            }
            Ok(out) => {
                overall_error = true;
                results.push(json!({
                    "hook": hook,
                    "status": "error",
                    "returncode": out.status.code(),
                    "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string(),
                }));
                break;
            }
            Err(e) => {
                overall_error = true;
                results.push(json!({
                    "hook": hook,
                    "status": "error",
                    "reason": format!("Failed to spawn shell command: {e}"),
                }));
                break;
            }
        }
    }

    let _ = fs::remove_dir_all(&tmp_dir);

    let final_transcript = current.trim().to_string();
    let out_txt = session_dir.join("transcript.txt");
    let _ = fs::write(
        &out_txt,
        if final_transcript.is_empty() {
            String::new()
        } else {
            format!("{}\n", final_transcript)
        },
    );

    let meta = json!({
        "status": if overall_error { "error" } else { "ok" },
        "count": hooks.len(),
        "hooks": results,
    });
    (final_transcript, meta)
}

#[cfg(test)]
mod hook_tests {
    use super::*;
    use crate::cli::Commands;
    use std::sync::Mutex;

    // RIFF_HOOKS is process-global; serialize tests that mutate it.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_cli() -> Cli {
        Cli {
            verbose: false,
            quiet: true,
            json: false,
            dry_run: false,
            no_beeps: true,
            command: Commands::Shot,
        }
    }

    fn default_stop_args() -> StopArgs {
        StopArgs {
            no_stop_hooks: false,
            no_hooks: false,
            with_post_hook: Vec::new(),
            transcribe_cmd: None,
            post_transcribe_cmd: None,
            python_bin: None,
            parakeet_script: None,
            parakeet_model: None,
        }
    }

    #[test]
    fn web_server_url_accepts_loopback_hosts() {
        assert_eq!(
            parse_web_server_loopback_url("http://127.0.0.1:8766").unwrap(),
            ("127.0.0.1".to_string(), "8766".to_string())
        );
        assert_eq!(
            parse_web_server_loopback_url("http://localhost:9000").unwrap(),
            ("localhost".to_string(), "9000".to_string())
        );
        assert_eq!(
            parse_web_server_loopback_url("http://LOCALHOST:9000").unwrap(),
            ("LOCALHOST".to_string(), "9000".to_string())
        );
        assert_eq!(
            parse_web_server_loopback_url("http://[::1]:8766").unwrap(),
            ("::1".to_string(), "8766".to_string())
        );
        assert_eq!(
            parse_web_server_loopback_url("http://[::1]").unwrap(),
            ("::1".to_string(), "8766".to_string())
        );
        // No explicit port falls back to the default.
        assert_eq!(
            parse_web_server_loopback_url("http://127.0.0.1").unwrap(),
            ("127.0.0.1".to_string(), "8766".to_string())
        );
    }

    #[test]
    fn web_server_url_rejects_non_loopback_or_malformed() {
        assert!(parse_web_server_loopback_url("http://example.com:8766").is_err());
        assert!(parse_web_server_loopback_url("http://0.0.0.0:8766").is_err());
        assert!(parse_web_server_loopback_url("https://127.0.0.1:8766").is_err());
        assert!(parse_web_server_loopback_url("127.0.0.1:8766").is_err());
        assert!(parse_web_server_loopback_url("http://user:pass@127.0.0.1:8766").is_err());
        assert!(parse_web_server_loopback_url("http://127.0.0.1:8766/evil").is_err());
        assert!(parse_web_server_loopback_url("http://127.0.0.1:notaport").is_err());
        assert!(parse_web_server_loopback_url("http://127.0.0.1:99999").is_err());
        assert!(parse_web_server_loopback_url("http://[::1").is_err());
        assert!(parse_web_server_loopback_url("http://").is_err());
    }

    #[test]
    fn output_hook_receives_temp_paths_and_rewrites_transcript() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session_dir = tempfile::tempdir().expect("session dir");
        let prev = env::var_os("RIFF_HOOKS");
        // $1 is the transcript temp file, $2 is the metadata temp file.
        // Assert the metadata file exists and is valid JSON, then strip "um".
        env::set_var(
            "RIFF_HOOKS",
            r#"test -f "$2" && grep -q session_id "$2" && perl -0777 -i -pe 's/\bum\b[,.]?//gi; s/[ \t]{2,}/ /g' "$1""#,
        );

        let metadata = json!({"session_id": "sess-123", "screenshots": []});
        let (out, meta) = run_output_hooks(
            "Um, hello um there.",
            &metadata,
            session_dir.path(),
            &default_stop_args(),
            &test_cli(),
        );

        match prev {
            Some(v) => env::set_var("RIFF_HOOKS", v),
            None => env::remove_var("RIFF_HOOKS"),
        }

        assert_eq!(meta["status"], "ok");
        assert_eq!(meta["count"], 1);
        assert_eq!(out, "hello there.");
        // final transcript persisted to the canonical file
        let written = fs::read_to_string(session_dir.path().join("transcript.txt")).unwrap();
        assert_eq!(written.trim(), "hello there.");
    }

    #[test]
    fn output_hook_skipped_when_not_configured() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session_dir = tempfile::tempdir().expect("session dir");
        let prev = env::var_os("RIFF_HOOKS");
        env::remove_var("RIFF_HOOKS");

        let (out, meta) = run_output_hooks(
            "unchanged",
            &json!({}),
            session_dir.path(),
            &default_stop_args(),
            &test_cli(),
        );

        if let Some(v) = prev {
            env::set_var("RIFF_HOOKS", v);
        }

        assert_eq!(out, "unchanged");
        assert_eq!(meta["status"], "skipped");
        assert_eq!(meta["reason"], "not_configured");
    }

    #[test]
    fn output_hook_skipped_when_disabled_by_flag() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session_dir = tempfile::tempdir().expect("session dir");
        let mut args = default_stop_args();
        args.no_stop_hooks = true;

        let (out, meta) = run_output_hooks(
            "unchanged",
            &json!({}),
            session_dir.path(),
            &args,
            &test_cli(),
        );

        assert_eq!(out, "unchanged");
        assert_eq!(meta["status"], "skipped");
        assert_eq!(meta["reason"], "disabled_by_flag");
    }

    #[test]
    fn output_hook_skipped_by_no_hooks_flag_even_when_configured() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session_dir = tempfile::tempdir().expect("session dir");
        let prev = env::var_os("RIFF_HOOKS");
        env::set_var("RIFF_HOOKS", r#"perl -0777 -i -pe 's/\bum\b//gi' "$1""#);

        let mut args = default_stop_args();
        args.no_hooks = true;

        let (out, meta) = run_output_hooks(
            "um unchanged um",
            &json!({}),
            session_dir.path(),
            &args,
            &test_cli(),
        );

        match prev {
            Some(v) => env::set_var("RIFF_HOOKS", v),
            None => env::remove_var("RIFF_HOOKS"),
        }

        assert_eq!(out, "um unchanged um");
        assert_eq!(meta["status"], "skipped");
        assert_eq!(meta["reason"], "disabled_by_flag");
    }

    #[test]
    fn normalize_cli_hook_appends_args_for_bare_path() {
        assert_eq!(normalize_cli_hook("/tmp/hook.sh"), r#"/tmp/hook.sh "$@""#);
        // A value that references the temp paths is left untouched.
        assert_eq!(
            normalize_cli_hook(r#"perl -i -pe 's/a/b/' "$1""#),
            r#"perl -i -pe 's/a/b/' "$1""#
        );
    }

    #[test]
    fn cli_post_hooks_run_after_env_hooks() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session_dir = tempfile::tempdir().expect("session dir");
        let prev = env::var_os("RIFF_HOOKS");
        // Env hook lowercases; CLI hook then uppercases. Order proves chaining.
        env::set_var(
            "RIFF_HOOKS",
            r#"tr '[:upper:]' '[:lower:]' < "$1" > "$1.t" && mv "$1.t" "$1""#,
        );

        let mut args = default_stop_args();
        args.with_post_hook =
            vec![r#"tr '[:lower:]' '[:upper:]' < "$1" > "$1.t" && mv "$1.t" "$1""#.to_string()];

        let (out, meta) = run_output_hooks(
            "Hello There",
            &json!({}),
            session_dir.path(),
            &args,
            &test_cli(),
        );

        match prev {
            Some(v) => env::set_var("RIFF_HOOKS", v),
            None => env::remove_var("RIFF_HOOKS"),
        }

        assert_eq!(meta["status"], "ok");
        assert_eq!(meta["count"], 2);
        assert_eq!(out, "HELLO THERE");
    }

    #[test]
    fn cli_post_hook_runs_even_with_no_hooks_flag() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session_dir = tempfile::tempdir().expect("session dir");
        let prev = env::var_os("RIFF_HOOKS");
        env::set_var("RIFF_HOOKS", r#"perl -0777 -i -pe 's/\bum\b//gi' "$1""#);

        let mut args = default_stop_args();
        args.no_hooks = true; // disables the env chain
        args.with_post_hook = vec![r#"perl -0777 -i -pe 's/there/world/gi' "$1""#.to_string()];

        let (out, meta) = run_output_hooks(
            "um hello there",
            &json!({}),
            session_dir.path(),
            &args,
            &test_cli(),
        );

        match prev {
            Some(v) => env::set_var("RIFF_HOOKS", v),
            None => env::remove_var("RIFF_HOOKS"),
        }

        // env hook skipped (um remains), CLI hook applied (there -> world)
        assert_eq!(meta["status"], "ok");
        assert_eq!(meta["count"], 1);
        assert_eq!(out, "um hello world");
    }
}
