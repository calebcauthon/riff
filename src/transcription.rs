use crate::cli::{Cli, StopArgs};
use crate::error::{app_error, AppError};
use crate::models::SessionState;
use crate::paths::{
    parakeet_server_log_file, parakeet_server_pid_file, parakeet_server_socket_file, root_dir,
    web_server_log_file, web_server_pid_file,
};
use crate::setup::default_user_runtime_dir;
use crate::{
    command_exists, fill_template, fill_template_with_transcript, print_verbose, process_is_alive,
    read_pid_file, round3, send_signal, shell_escape, write_pid_file,
};
use serde::{Deserialize, Serialize};
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
const DEFAULT_PARAKEET_MODEL_REVISION: &str = "main";
const DEFAULT_PARAKEET_SERVER_WAIT_READY_SEC: u64 = 30;
const PARAKEET_SERVER_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ParakeetServerIdentity {
    protocol_version: u32,
    service: String,
    server_instance_id: String,
    pid: i32,
    riff_root: String,
    script_path: String,
    transport: String,
    endpoint: String,
    model: String,
    model_revision: String,
    requested_device: String,
    device: String,
    python_version: String,
    python_executable: String,
    nemo_version: String,
    torch_version: String,
    started_at_epoch: f64,
}

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
    env::var("RIFF_PARAKEET_SERVER_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("unix://{}", normalized_path(&parakeet_server_socket_file())))
}

fn resolve_parakeet_model_revision() -> String {
    env::var("RIFF_PARAKEET_MODEL_REVISION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_PARAKEET_MODEL_REVISION.to_string())
}

fn resolve_parakeet_requested_device() -> String {
    env::var("RIFF_PARAKEET_DEVICE")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| matches!(v.as_str(), "auto" | "cpu" | "cuda"))
        .unwrap_or_else(|| "auto".to_string())
}

fn normalized_path(path: &Path) -> String {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical.display().to_string();
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(canonical_parent) = fs::canonicalize(parent) {
            return canonical_parent.join(name).display().to_string();
        }
    }
    path.display().to_string()
}

fn parakeet_server_unix_socket(base_url: &str) -> Option<PathBuf> {
    base_url.strip_prefix("unix://").map(PathBuf::from)
}

fn parakeet_server_request_url(base_url: &str, route: &str) -> String {
    if parakeet_server_unix_socket(base_url).is_some() {
        format!("http://localhost/{route}")
    } else {
        format!("{}/{route}", base_url.trim_end_matches('/'))
    }
}

fn parakeet_curl_command(base_url: &str) -> Command {
    let mut cmd = Command::new("curl");
    if let Some(socket_path) = parakeet_server_unix_socket(base_url) {
        cmd.arg("--unix-socket").arg(socket_path);
    }
    cmd
}

fn parakeet_server_wait_ready_timeout_sec() -> u64 {
    env::var("RIFF_PARAKEET_SERVER_WAIT_READY_SEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_PARAKEET_SERVER_WAIT_READY_SEC)
}

fn parakeet_server_health_url(base: &str) -> String {
    parakeet_server_request_url(base, "health")
}

fn parakeet_server_transcribe_url(base: &str) -> String {
    parakeet_server_request_url(base, "transcribe")
}

fn query_parakeet_server_identity(base_url: &str) -> Result<ParakeetServerIdentity, String> {
    if !command_exists("curl") {
        return Err("curl not found".to_string());
    }

    let out = parakeet_curl_command(base_url)
        .args(["-sS", "--max-time", "0.5", "--fail"])
        .arg(parakeet_server_health_url(base_url))
        .output()
        .map_err(|e| format!("health request failed: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "health request returned {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let parsed: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("health returned invalid JSON: {e}"))?;
    if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!("health returned ok=false: {parsed}"));
    }
    serde_json::from_value(parsed).map_err(|e| format!("health identity is incomplete: {e}"))
}

fn validate_parakeet_server_identity(
    identity: &ParakeetServerIdentity,
    model: Option<&str>,
    script_path: Option<&Path>,
) -> Result<(), String> {
    let expected_root = normalized_path(&root_dir());
    let expected_revision = resolve_parakeet_model_revision();
    let expected_device = resolve_parakeet_requested_device();
    let mut mismatches = Vec::new();

    if identity.protocol_version != PARAKEET_SERVER_PROTOCOL_VERSION {
        mismatches.push(format!(
            "protocol_version expected={} actual={}",
            PARAKEET_SERVER_PROTOCOL_VERSION, identity.protocol_version
        ));
    }
    if identity.service != "parakeet" {
        mismatches.push(format!(
            "service expected=parakeet actual={}",
            identity.service
        ));
    }
    if identity.riff_root != expected_root {
        mismatches.push(format!(
            "riff_root expected={expected_root} actual={}",
            identity.riff_root
        ));
    }
    if let Some(model) = model {
        if identity.model != model {
            mismatches.push(format!("model expected={model} actual={}", identity.model));
        }
        if identity.model_revision != expected_revision {
            mismatches.push(format!(
                "model_revision expected={expected_revision} actual={}",
                identity.model_revision
            ));
        }
        if identity.requested_device != expected_device {
            mismatches.push(format!(
                "requested_device expected={expected_device} actual={}",
                identity.requested_device
            ));
        }
        if expected_device != "auto" && identity.device != expected_device {
            mismatches.push(format!(
                "device expected={expected_device} actual={}",
                identity.device
            ));
        }
    }
    if let Some(script_path) = script_path {
        let expected_script = normalized_path(script_path);
        if identity.script_path != expected_script {
            mismatches.push(format!(
                "script_path expected={expected_script} actual={}",
                identity.script_path
            ));
        }
    }
    for (field, value) in [
        ("server_instance_id", identity.server_instance_id.as_str()),
        ("device", identity.device.as_str()),
        ("python_version", identity.python_version.as_str()),
        ("python_executable", identity.python_executable.as_str()),
        ("nemo_version", identity.nemo_version.as_str()),
        ("torch_version", identity.torch_version.as_str()),
    ] {
        if value.trim().is_empty() {
            mismatches.push(format!("{field} is empty"));
        }
    }

    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(mismatches.join(", "))
    }
}

fn validate_parakeet_server_endpoint(
    identity: &ParakeetServerIdentity,
    base_url: &str,
) -> Result<(), String> {
    let expected_endpoint = base_url.trim_end_matches('/');
    let expected_transport = if parakeet_server_unix_socket(base_url).is_some() {
        "unix"
    } else {
        "tcp"
    };
    if identity.transport != expected_transport {
        return Err(format!(
            "transport expected={expected_transport} actual={}",
            identity.transport
        ));
    }
    if identity.endpoint.trim_end_matches('/') != expected_endpoint {
        return Err(format!(
            "endpoint expected={expected_endpoint} actual={}",
            identity.endpoint
        ));
    }
    Ok(())
}

fn get_valid_parakeet_server_identity(
    base_url: &str,
    model: Option<&str>,
    script_path: Option<&Path>,
) -> Result<ParakeetServerIdentity, String> {
    let identity = query_parakeet_server_identity(base_url)?;
    validate_parakeet_server_identity(&identity, model, script_path)?;
    validate_parakeet_server_endpoint(&identity, base_url)?;
    Ok(identity)
}

pub(crate) fn check_parakeet_server_health(base_url: &str) -> bool {
    get_valid_parakeet_server_identity(base_url, None, None).is_ok()
}

fn identity_owned_by_current_root(identity: &ParakeetServerIdentity, base_url: &str) -> bool {
    identity.protocol_version == PARAKEET_SERVER_PROTOCOL_VERSION
        && identity.service == "parakeet"
        && identity.riff_root == normalized_path(&root_dir())
        && identity.transport == "unix"
        && identity.endpoint == base_url
}

fn stop_mismatched_owned_server(
    identity: &ParakeetServerIdentity,
    base_url: &str,
    cli: &Cli,
) -> bool {
    if !identity_owned_by_current_root(identity, base_url) || identity.pid <= 0 {
        return false;
    }
    print_verbose(
        cli,
        format!(
            "Stopping mismatched Riff-owned Parakeet server pid={} instance={} model={} device={}",
            identity.pid, identity.server_instance_id, identity.model, identity.device
        ),
    );
    if send_signal(identity.pid, libc::SIGTERM).is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !process_is_alive(identity.pid) {
            let _ = fs::remove_file(parakeet_server_pid_file());
            let _ = fs::remove_file(parakeet_server_socket_file());
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn process_matches_parakeet_server(pid: i32, script_path: &Path, model: &str) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    command.contains(&normalized_path(script_path))
        && command.contains("--serve")
        && command.contains(model)
}

fn spawn_parakeet_server(
    python_bin: &str,
    script_path: &Path,
    model: &str,
    cli: &Cli,
) -> Result<i32, AppError> {
    let pid_file = parakeet_server_pid_file();
    let base_url = parakeet_server_base_url();

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

    let mut command = Command::new(python_bin);
    command
        .arg(script_path)
        .arg("--serve")
        .arg("--model")
        .arg(model)
        .arg("--device")
        .arg(resolve_parakeet_requested_device())
        .arg("--model-revision")
        .arg(resolve_parakeet_model_revision())
        .arg("--riff-root")
        .arg(normalized_path(&root_dir()));
    if let Some(socket_path) = parakeet_server_unix_socket(&base_url) {
        command.arg("--unix-socket").arg(socket_path);
    } else {
        let host_port = base_url
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let mut parts = host_port.split(':');
        command
            .arg("--host")
            .arg(parts.next().unwrap_or("127.0.0.1"))
            .arg("--port")
            .arg(parts.next().unwrap_or("8765"));
    }
    let child = command
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
) -> Option<ParakeetServerIdentity> {
    if !parakeet_server_enabled() {
        return None;
    }

    let base_url = parakeet_server_base_url();
    match get_valid_parakeet_server_identity(&base_url, Some(model), Some(script_path)) {
        Ok(identity) => return Some(identity),
        Err(reason) => print_verbose(
            cli,
            format!("Parakeet server is not usable at {base_url}: {reason}"),
        ),
    }

    let mut spawned_pid = None;
    if let Ok(identity) = query_parakeet_server_identity(&base_url) {
        if identity_owned_by_current_root(&identity, &base_url) {
            if !stop_mismatched_owned_server(&identity, &base_url, cli) {
                print_verbose(cli, "Could not stop mismatched Riff-owned Parakeet server.");
                return None;
            }
        } else {
            print_verbose(
                cli,
                format!(
                    "Refusing to replace Parakeet server not owned by RIFF_ROOT {}: {}",
                    normalized_path(&root_dir()),
                    serde_json::to_string(&identity).unwrap_or_default()
                ),
            );
            return None;
        }
    } else if let Some(pid) = read_pid_file(&parakeet_server_pid_file()) {
        if process_is_alive(pid) && process_matches_parakeet_server(pid, script_path, model) {
            spawned_pid = Some(pid);
            print_verbose(
                cli,
                format!("Waiting for existing Parakeet server process pid={pid}"),
            );
        } else if process_is_alive(pid) {
            print_verbose(
                cli,
                format!("Ignoring PID file for unrelated process pid={pid}"),
            );
        }
    }

    if spawned_pid.is_none() {
        spawned_pid = match spawn_parakeet_server(python_bin, script_path, model, cli) {
            Ok(pid) => Some(pid),
            Err(e) => {
                print_verbose(
                    cli,
                    format!("Failed to start Parakeet server: {}", e.message),
                );
                None
            }
        };
    }
    if !wait_ready {
        return None;
    }

    let deadline = Instant::now() + Duration::from_secs(parakeet_server_wait_ready_timeout_sec());
    let mut saw_dead_server_process = false;
    let mut restarted_mismatched_server = false;
    while Instant::now() < deadline {
        if let Ok(identity) = query_parakeet_server_identity(&base_url) {
            match validate_parakeet_server_identity(&identity, Some(model), Some(script_path)) {
                Ok(()) => {
                    print_verbose(
                        cli,
                        format!(
                            "Parakeet server ready at {} (pid={}, instance={}, model={}, device={})",
                            base_url,
                            identity.pid,
                            identity.server_instance_id,
                            identity.model,
                            identity.device
                        ),
                    );
                    return Some(identity);
                }
                Err(reason)
                    if identity_owned_by_current_root(&identity, &base_url)
                        && !restarted_mismatched_server =>
                {
                    print_verbose(
                        cli,
                        format!("Replacing mismatched Parakeet server after startup: {reason}"),
                    );
                    if !stop_mismatched_owned_server(&identity, &base_url, cli) {
                        break;
                    }
                    restarted_mismatched_server = true;
                    spawned_pid = match spawn_parakeet_server(python_bin, script_path, model, cli) {
                        Ok(pid) => Some(pid),
                        Err(e) => {
                            print_verbose(
                                cli,
                                format!("Failed to restart Parakeet server: {}", e.message),
                            );
                            break;
                        }
                    };
                    continue;
                }
                Err(reason) => {
                    print_verbose(cli, format!("Parakeet server identity mismatch: {reason}"));
                    if !identity_owned_by_current_root(&identity, &base_url) {
                        break;
                    }
                }
            }
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
    None
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

pub(crate) fn transcribe_via_parakeet_server(
    base_url: &str,
    audio_path: &Path,
    out_txt: &Path,
    model: &str,
    expected_identity: &ParakeetServerIdentity,
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
        "protocol_version": PARAKEET_SERVER_PROTOCOL_VERSION,
        "server_instance_id": expected_identity.server_instance_id,
        "riff_root": normalized_path(&root_dir()),
        "model": model,
        "model_revision": resolve_parakeet_model_revision(),
        "requested_device": resolve_parakeet_requested_device(),
        "device": expected_identity.device,
    })
    .to_string();

    let out = parakeet_curl_command(base_url)
        .args([
            "-sS",
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

    let actual_identity = match parsed
        .get("server")
        .cloned()
        .ok_or_else(|| "missing server identity".to_string())
        .and_then(|value| {
            serde_json::from_value::<ParakeetServerIdentity>(value)
                .map_err(|e| format!("invalid server identity: {e}"))
        }) {
        Ok(identity) => identity,
        Err(reason) => {
            return Err(json!({
                "status": "error",
                "method": "parakeet_server",
                "reason": reason,
                "response": parsed,
            }))
        }
    };
    if let Err(reason) = validate_parakeet_server_identity(&actual_identity, Some(model), None) {
        return Err(json!({
            "status": "error",
            "method": "parakeet_server",
            "reason": format!("response server identity mismatch: {reason}"),
            "server_identity": actual_identity,
        }));
    }
    if let Err(reason) = validate_parakeet_server_endpoint(&actual_identity, base_url) {
        return Err(json!({
            "status": "error",
            "method": "parakeet_server",
            "reason": format!("response server endpoint mismatch: {reason}"),
            "server_identity": actual_identity,
        }));
    }
    if actual_identity.server_instance_id != expected_identity.server_instance_id {
        return Err(json!({
            "status": "error",
            "method": "parakeet_server",
            "reason": "server instance changed between health check and transcription",
            "expected_instance_id": expected_identity.server_instance_id,
            "actual_instance_id": actual_identity.server_instance_id,
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
            "model": actual_identity.model,
            "model_revision": actual_identity.model_revision,
            "device": actual_identity.device,
            "server_identity": actual_identity,
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
    perf.insert("python_bin".to_string(), json!(python_bin.clone()));
    perf.insert("model".to_string(), json!(model.clone()));
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
        let identity_before =
            get_valid_parakeet_server_identity(&base_url, Some(&model), Some(&script_path));
        let server_health_before = identity_before.is_ok();
        perf_mark(
            &mut perf,
            "server_health_before_check_ms",
            t_server_health_before,
        );
        perf.insert(
            "server_health_before".to_string(),
            json!(server_health_before),
        );
        match &identity_before {
            Ok(identity) => {
                perf.insert("server_identity_before".to_string(), json!(identity));
            }
            Err(reason) => {
                perf.insert("server_health_before_error".to_string(), json!(reason));
            }
        }

        let t_server_ensure = Instant::now();
        let server_identity = match identity_before {
            Ok(identity) => Some(identity),
            Err(_) => ensure_parakeet_server(&python_bin, &script_path, &model, cli, true),
        };
        perf_mark(&mut perf, "server_ensure_ms", t_server_ensure);

        let server_health_after = server_identity.is_some();
        perf.insert("server_health_after_check_ms".to_string(), json!(0.0));
        perf.insert(
            "server_health_after".to_string(),
            json!(server_health_after),
        );

        if let Some(identity) = &server_identity {
            perf.insert("server_identity".to_string(), json!(identity));
            perf.insert("server_pid".to_string(), json!(identity.pid));
            perf.insert(
                "server_pid_alive".to_string(),
                json!(process_is_alive(identity.pid)),
            );
        }

        if let Some(identity) = server_identity {
            let t_server_request = Instant::now();
            match transcribe_via_parakeet_server(
                &base_url,
                &audio_path,
                &out_txt,
                &model,
                &identity,
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

    let t_python = Instant::now();
    let output = Command::new(&python_bin)
        .arg(&script_path)
        .arg("--audio")
        .arg(&audio_path)
        .arg("--out-txt")
        .arg(&out_txt)
        .arg("--model")
        .arg(&model)
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

    fn matching_server_identity(model: &str, endpoint: &str) -> ParakeetServerIdentity {
        let requested_device = resolve_parakeet_requested_device();
        ParakeetServerIdentity {
            protocol_version: PARAKEET_SERVER_PROTOCOL_VERSION,
            service: "parakeet".to_string(),
            server_instance_id: "test-instance".to_string(),
            pid: std::process::id() as i32,
            riff_root: normalized_path(&root_dir()),
            script_path: normalized_path(Path::new(file!())),
            transport: "unix".to_string(),
            endpoint: endpoint.to_string(),
            model: model.to_string(),
            model_revision: resolve_parakeet_model_revision(),
            requested_device: requested_device.clone(),
            device: if requested_device == "cuda" {
                "cuda".to_string()
            } else {
                "cpu".to_string()
            },
            python_version: "3.12.0".to_string(),
            python_executable: "/test/python".to_string(),
            nemo_version: "2.4.0".to_string(),
            torch_version: "2.7.1".to_string(),
            started_at_epoch: 1.0,
        }
    }

    #[test]
    fn parakeet_request_urls_support_unix_and_tcp() {
        assert_eq!(
            parakeet_server_request_url("unix:///tmp/riff.sock", "health"),
            "http://localhost/health"
        );
        assert_eq!(
            parakeet_server_request_url("http://127.0.0.1:8765/", "transcribe"),
            "http://127.0.0.1:8765/transcribe"
        );
    }

    #[test]
    fn parakeet_identity_validation_rejects_wrong_model() {
        let endpoint = format!("unix://{}", parakeet_server_socket_file().display());
        let mut identity = matching_server_identity("model-a", &endpoint);
        assert!(validate_parakeet_server_identity(&identity, Some("model-a"), None).is_ok());

        identity.model = "model-b".to_string();
        let error = validate_parakeet_server_identity(&identity, Some("model-a"), None)
            .expect_err("wrong model must be rejected");
        assert!(error.contains("model expected=model-a actual=model-b"));
    }

    #[test]
    fn only_current_root_unix_socket_is_treated_as_owned() {
        let endpoint = format!("unix://{}", normalized_path(&parakeet_server_socket_file()));
        let mut identity = matching_server_identity("model-a", &endpoint);
        assert!(identity_owned_by_current_root(&identity, &endpoint));

        identity.transport = "tcp".to_string();
        identity.endpoint = "http://127.0.0.1:8765".to_string();
        assert!(!identity_owned_by_current_root(
            &identity,
            "http://127.0.0.1:8765"
        ));
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
