use crate::cli::{Cli, StopArgs};
use crate::error::{app_error, AppError};
use crate::models::SessionState;
use crate::paths::{
    parakeet_server_log_file, parakeet_server_pid_file, root_dir, web_server_log_file,
    web_server_pid_file,
};
use crate::{
    command_exists, fill_template, print_verbose, process_is_alive, read_pid_file, round3,
    shell_escape, write_pid_file,
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

const DEFAULT_PARAKEET_MODEL: &str = "nvidia/parakeet-tdt-0.6b-v2";
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

fn search_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    if let Ok(cwd) = env::current_dir() {
        roots.push(cwd);
    }

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

fn first_existing_relative(paths: &[&str]) -> Option<PathBuf> {
    for root in search_roots() {
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

fn parakeet_server_base_url() -> String {
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

fn default_web_server_script() -> Option<PathBuf> {
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
