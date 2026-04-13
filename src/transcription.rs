use crate::cli::{Cli, StopArgs};
use crate::error::{app_error, AppError};
use crate::models::SessionState;
use crate::paths::{
    parakeet_server_log_file, parakeet_server_pid_file, root_dir, web_server_log_file,
    web_server_pid_file,
};
use crate::{
    command_exists, fill_template, print_verbose, process_is_alive, read_pid_file, shell_escape,
    write_pid_file,
};
use serde_json::{json, Value};
use std::env;
use std::fs::{self, OpenOptions};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn default_parakeet_script() -> Option<PathBuf> {
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

pub(crate) fn parakeet_server_enabled() -> bool {
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

pub(crate) fn web_server_base_url() -> String {
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

pub(crate) fn default_sound_picker_script() -> Option<PathBuf> {
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

pub(crate) fn ensure_web_server(cli: &Cli, wait_ready: bool) -> bool {
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

pub(crate) fn run_transcription(
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
