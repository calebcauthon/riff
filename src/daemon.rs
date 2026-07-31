//! The riff daemon (`riffd`): a control socket for events in and out.
//!
//! `riff daemon run` listens on `$RIFF_ROOT/riffd.sock` (mode 0600) and speaks
//! minimal HTTP/1.1, following the Parakeet server's transport and identity
//! conventions so `curl --unix-socket` works for debugging:
//!
//! - `GET /identity` — who owns this socket (root, pid, version)
//! - `GET /health`   — identity plus uptime and subscriber count
//! - `POST /events`  — append an external event; the daemon assigns the
//!   envelope with `command: "external:<source>"`
//! - `GET /subscribe` — long-lived NDJSON stream of bus events
//!
//! The JSONL bus file stays the durable log and the single source of truth:
//! `/subscribe` is fed by tailing it, so events appended by any process —
//! one-shot CLI invocations, Python helpers, the daemon itself — all reach
//! subscribers. The daemon never gates or rewrites what other processes write.

use crate::bus::{BusTailer, EventFilters};
use crate::cli::{Cli, DaemonAction, DaemonArgs, DaemonRunArgs, EmitArgs};
use crate::error::{app_error, AppError};
use crate::events;
use crate::models::SessionState;
use crate::paths::{
    active_state_file, ensure_dirs, riffd_log_file, riffd_pid_file, riffd_socket_file, root_dir,
};
use crate::transcription::normalized_path;
use crate::{
    emit_json, now_iso, print_out, print_verbose, process_is_alive, read_json, read_pid_file,
    write_pid_file, RIFF_BUILD_ID, RIFF_VERSION,
};
use serde_json::{json, Map, Value};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: u64 = 1;
const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

type Hub = Arc<Mutex<Vec<mpsc::Sender<Arc<String>>>>>;

// ---------------------------------------------------------------------------
// Command entry points
// ---------------------------------------------------------------------------

pub(crate) fn cmd_daemon(cli: &Cli, args: &DaemonArgs) -> Result<i32, AppError> {
    match &args.action {
        DaemonAction::Run(run_args) => run_daemon(cli, run_args),
        DaemonAction::Start(start_args) => daemon_start(cli, !start_args.no_wait),
        DaemonAction::Stop => daemon_stop(cli),
        DaemonAction::Status => daemon_status(cli),
    }
}

/// `riff emit`: append an event directly to the bus with a native envelope.
/// Riff processes never need the daemon to publish — the file is the bus.
/// `POST /events` exists for tools that are not riff.
pub(crate) fn cmd_emit(cli: &Cli, args: &EmitArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    validate_event_type(&args.event_type).map_err(|reason| app_error(2, reason))?;
    let level = validate_level(&args.level).map_err(|reason| app_error(2, reason))?;

    let payload = match args.data.as_deref() {
        None => Value::Object(Map::new()),
        Some(raw) => {
            let parsed: Value = serde_json::from_str(raw)
                .map_err(|e| app_error(2, format!("--data is not valid JSON: {e}")))?;
            if !parsed.is_object() {
                return Err(app_error(2, "--data must be a JSON object."));
            }
            parsed
        }
    };

    let session_id = match args.session.as_deref() {
        Some("current") | Some("active") => {
            let active = active_state_file();
            if !active.exists() {
                return Err(app_error(2, "No active session for --session current."));
            }
            let state: SessionState = read_json(&active)?;
            Some(state.session_id)
        }
        Some(other) => Some(other.to_string()),
        None => None,
    };

    events::emit_leveled(level, &args.event_type, session_id.as_deref(), payload);

    if !cli.quiet && !cli.json {
        print_out(cli, format!("emitted {}", args.event_type));
    }
    emit_json(
        cli,
        &json!({
            "ok": true,
            "type": args.event_type,
            "session_id": session_id,
            "level": level,
        }),
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// Daemon lifecycle
// ---------------------------------------------------------------------------

extern "C" fn on_shutdown_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            on_shutdown_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            on_shutdown_signal as *const () as libc::sighandler_t,
        );
    }
}

fn run_daemon(cli: &Cli, args: &DaemonRunArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let our_root = normalized_path(&root_dir());
    if let Some(claimed) = &args.root {
        let claimed = normalized_path(claimed);
        if claimed != our_root {
            return Err(app_error(
                2,
                format!("--root {claimed} does not match RIFF_ROOT {our_root}."),
            ));
        }
    }

    let socket_path = riffd_socket_file();
    if socket_path.exists() {
        if let Some(identity) = query_identity() {
            let pid = identity.get("pid").and_then(|v| v.as_i64()).unwrap_or(-1);
            return Err(app_error(
                2,
                format!("riffd is already running (pid={pid})."),
            ));
        }
        // Nothing answered: a previous daemon died without cleanup.
        let _ = fs::remove_file(&socket_path);
    }

    install_signal_handlers();
    SHUTDOWN.store(false, Ordering::SeqCst);

    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| app_error(1, format!("Failed to bind {}: {e}", socket_path.display())))?;
    let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600));
    listener
        .set_nonblocking(true)
        .map_err(|e| app_error(1, format!("Failed to set listener nonblocking: {e}")))?;
    write_pid_file(&riffd_pid_file(), std::process::id() as i32);

    let started_at = Instant::now();
    let started_at_iso = now_iso();
    let hub: Hub = Arc::new(Mutex::new(Vec::new()));

    // Feed subscribers by tailing the bus file, so events appended by every
    // process reach them, not only the daemon's own.
    {
        let hub = Arc::clone(&hub);
        thread::spawn(move || {
            let mut tailer = BusTailer::from_end();
            while !SHUTDOWN.load(Ordering::SeqCst) {
                let lines = tailer.poll();
                if !lines.is_empty() {
                    let mut subs = hub.lock().unwrap_or_else(|e| e.into_inner());
                    for line in lines {
                        let line = Arc::new(line);
                        subs.retain(|sub| sub.send(Arc::clone(&line)).is_ok());
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
    }

    events::emit(
        "daemon_started",
        None,
        json!({ "socket": socket_path.display().to_string(), "root": our_root }),
    );
    print_out(
        cli,
        format!(
            "riffd listening at {} — Ctrl-C to stop",
            socket_path.display()
        ),
    );

    // Warm the transcription server now, in the background, so the first
    // `stop` never pays the model-load cost. The socket is already serving, so
    // this never delays daemon readiness.
    if crate::bool_env_enabled("RIFF_DAEMON_PRELOAD", true) {
        thread::spawn(|| {
            use clap::Parser;
            let quiet_cli = Cli::parse_from(["riff", "--quiet", "daemon", "run"]);
            let report =
                crate::servers::start_helper(&quiet_cli, crate::servers::Helper::Parakeet, true);
            let status = report
                .get("status")
                .or_else(|| report.get("outcome"))
                .cloned()
                .unwrap_or(Value::Null);
            events::emit(
                "daemon_preload",
                None,
                json!({ "server": "parakeet", "status": status }),
            );
        });
    }

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let hub = Arc::clone(&hub);
                let started_at_iso = started_at_iso.clone();
                thread::spawn(move || {
                    handle_connection(stream, hub, started_at, started_at_iso);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }

    events::emit("daemon_stopped", None, json!({ "root": our_root }));
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(riffd_pid_file());
    print_verbose(cli, "riffd stopped.");
    Ok(0)
}

fn daemon_start(cli: &Cli, wait_ready: bool) -> Result<i32, AppError> {
    ensure_dirs()?;

    if let Some(identity) = identity_for_current_root() {
        let pid = identity.get("pid").and_then(|v| v.as_i64());
        if !cli.quiet && !cli.json {
            print_out(
                cli,
                format!("riffd already running (pid={}).", pid.unwrap_or(-1)),
            );
        }
        emit_json(
            cli,
            &json!({ "ok": true, "status": "already_running", "daemon_pid": pid }),
        );
        return Ok(0);
    }

    let exe = std::env::current_exe()
        .map_err(|e| app_error(1, format!("Failed to resolve riff binary: {e}")))?;
    let log_path = riffd_log_file();
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| app_error(1, format!("Failed to open {}: {e}", log_path.display())))?;
    let log_file_err = log_file
        .try_clone()
        .map_err(|e| app_error(1, format!("Failed to clone daemon log handle: {e}")))?;

    let child = Command::new(exe)
        .arg("daemon")
        .arg("run")
        .arg("--root")
        .arg(normalized_path(&root_dir()))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
        .map_err(|e| app_error(1, format!("Failed to start riffd: {e}")))?;
    let spawned_pid = child.id() as i64;

    if !wait_ready {
        emit_json(
            cli,
            &json!({ "ok": true, "status": "spawned", "daemon_pid": spawned_pid }),
        );
        if !cli.quiet && !cli.json {
            print_out(cli, format!("riffd spawned (pid={spawned_pid})."));
        }
        return Ok(0);
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(identity) = identity_for_current_root() {
            let pid = identity.get("pid").and_then(|v| v.as_i64());
            if !cli.quiet && !cli.json {
                print_out(cli, format!("riffd ready (pid={}).", pid.unwrap_or(-1)));
            }
            emit_json(
                cli,
                &json!({ "ok": true, "status": "ready", "daemon_pid": pid }),
            );
            return Ok(0);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(app_error(
        1,
        format!(
            "riffd did not become ready within 5s; check {}",
            log_path.display()
        ),
    ))
}

fn daemon_stop(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;

    let pid = read_pid_file(&riffd_pid_file()).or_else(|| {
        identity_for_current_root()
            .and_then(|identity| identity.get("pid").and_then(|v| v.as_i64()))
            .map(|pid| pid as i32)
    });

    let Some(pid) = pid else {
        if !cli.quiet && !cli.json {
            print_out(cli, "riffd is not running.");
        }
        emit_json(cli, &json!({ "ok": true, "status": "not_running" }));
        return Ok(0);
    };

    let outcome = crate::servers::stop_pid(pid);
    if !process_is_alive(pid) {
        let _ = fs::remove_file(riffd_socket_file());
        let _ = fs::remove_file(riffd_pid_file());
    }
    if !cli.quiet && !cli.json {
        print_out(cli, format!("riffd stop: pid={pid} outcome={outcome}"));
    }
    emit_json(
        cli,
        &json!({ "ok": true, "status": outcome, "daemon_pid": pid }),
    );
    Ok(0)
}

fn daemon_status(cli: &Cli) -> Result<i32, AppError> {
    ensure_dirs()?;

    match query_health() {
        Some(health) => {
            if !cli.quiet && !cli.json {
                let pid = health.get("pid").and_then(|v| v.as_i64()).unwrap_or(-1);
                let uptime = health
                    .get("uptime_sec")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let subscribers = health
                    .get("subscribers")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                print_out(
                    cli,
                    format!(
                        "riffd running: pid={pid} uptime={uptime:.0}s subscribers={subscribers} socket={}",
                        riffd_socket_file().display()
                    ),
                );
            }
            emit_json(
                cli,
                &json!({ "ok": true, "running": true, "health": health }),
            );
            Ok(0)
        }
        None => {
            if !cli.quiet && !cli.json {
                print_out(cli, "riffd is not running.");
            }
            emit_json(cli, &json!({ "ok": true, "running": false }));
            Ok(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    body: Vec<u8>,
}

fn handle_connection(
    mut stream: UnixStream,
    hub: Hub,
    started_at: Instant,
    started_at_iso: String,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(reason) => {
            respond_json(&mut stream, 400, &json!({ "ok": false, "error": reason }));
            return;
        }
    };

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/identity") => {
            respond_json(&mut stream, 200, &identity_value(&started_at_iso));
        }
        ("GET", "/health") => {
            let subscribers = hub.lock().map(|subs| subs.len()).unwrap_or(0);
            let mut health = identity_value(&started_at_iso);
            if let Some(map) = health.as_object_mut() {
                map.insert("ok".to_string(), json!(true));
                map.insert(
                    "uptime_sec".to_string(),
                    json!(crate::round3(started_at.elapsed().as_secs_f64())),
                );
                map.insert("subscribers".to_string(), json!(subscribers));
            }
            respond_json(&mut stream, 200, &health);
        }
        ("POST", "/events") => match ingest_external_event(&request.body) {
            Ok(summary) => respond_json(&mut stream, 200, &summary),
            Err(reason) => respond_json(&mut stream, 400, &json!({ "ok": false, "error": reason })),
        },
        ("GET", "/subscribe") => serve_subscription(stream, &request.query, &hub),
        _ => respond_json(
            &mut stream,
            404,
            &json!({ "ok": false, "error": format!("No route {} {}", request.method, request.path) }),
        ),
    }
}

/// Append an external event with a daemon-assigned envelope. The sender only
/// chooses its `source` name, event type, level, session, and payload — the
/// envelope (and its clipping and collision rules) is riff's.
fn ingest_external_event(body: &[u8]) -> Result<Value, String> {
    let parsed: Value =
        serde_json::from_slice(body).map_err(|e| format!("Body is not valid JSON: {e}"))?;
    let Some(map) = parsed.as_object() else {
        return Err("Body must be a JSON object.".to_string());
    };

    let event_type = map
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing required string field 'type'.".to_string())?;
    validate_event_type(event_type)?;

    let source = match map.get("source") {
        None => "unknown",
        Some(value) => value
            .as_str()
            .ok_or_else(|| "'source' must be a string.".to_string())?,
    };
    validate_source(source)?;

    let level = match map.get("level") {
        None => events::LEVEL_INFO,
        Some(value) => validate_level(
            value
                .as_str()
                .ok_or_else(|| "'level' must be a string.".to_string())?,
        )?,
    };

    let session_id = match map.get("session_id") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| "'session_id' must be a string.".to_string())?,
        ),
    };

    let payload = match map.get("payload") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Err("'payload' must be a JSON object.".to_string()),
    };

    let command = format!("external:{source}");
    events::emit_as(&command, level, event_type, session_id, payload);
    Ok(json!({ "ok": true, "type": event_type, "command": command }))
}

fn serve_subscription(mut stream: UnixStream, query: &[(String, String)], hub: &Hub) {
    let filters = filters_from_query(query);
    let (sender, receiver) = mpsc::channel::<Arc<String>>();
    if let Ok(mut subs) = hub.lock() {
        subs.push(sender);
    }

    // Long-lived response: no content length, the stream ends when either side
    // closes. Clients read NDJSON lines.
    let header =
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n";
    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();

    loop {
        match receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if !filters.matches(&value, &line) {
                    continue;
                }
                if stream.write_all(line.as_bytes()).is_err()
                    || stream.write_all(b"\n").is_err()
                    || stream.flush().is_err()
                {
                    // Receiver drops here; the tailer prunes the dead sender.
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if SHUTDOWN.load(Ordering::SeqCst) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn filters_from_query(query: &[(String, String)]) -> EventFilters {
    let mut filters = EventFilters::default();
    for (key, value) in query {
        match key.as_str() {
            "type" => filters
                .types
                .extend(value.split(',').filter(|v| !v.is_empty()).map(String::from)),
            "command" => filters
                .commands
                .extend(value.split(',').filter(|v| !v.is_empty()).map(String::from)),
            "session" => filters.session = Some(value.clone()),
            "grep" => filters.grep = Some(value.clone()),
            _ => {}
        }
    }
    filters
}

fn identity_value(started_at_iso: &str) -> Value {
    json!({
        "service": "riffd",
        "protocol_version": PROTOCOL_VERSION,
        "pid": std::process::id(),
        "riff_root": normalized_path(&root_dir()),
        "version": RIFF_VERSION,
        "build_id": RIFF_BUILD_ID,
        "started_at": started_at_iso,
        "socket": riffd_socket_file().display().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1
// ---------------------------------------------------------------------------

fn read_request(stream: &mut UnixStream) -> Result<Request, String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // Byte-at-a-time until the blank line; request heads are tiny and this
    // avoids buffering past the head into the body.
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > MAX_HEAD_BYTES {
            return Err("Request head too large.".to_string());
        }
        match stream.read(&mut byte) {
            Ok(0) => return Err("Connection closed mid-request.".to_string()),
            Ok(_) => head.push(byte[0]),
            Err(e) => return Err(format!("Failed to read request: {e}")),
        }
    }

    let head_text = String::from_utf8_lossy(&head);
    let mut lines = head_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_uppercase();
    let target = parts.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() {
        return Err(format!("Malformed request line: {request_line}"));
    }

    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err("Request body too large.".to_string());
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream
            .read_exact(&mut body)
            .map_err(|e| format!("Failed to read request body: {e}"))?;
    }

    let (path, query_text) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query),
        None => (target.to_string(), ""),
    };
    let query = query_text
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (pair.to_string(), String::new()),
        })
        .collect();

    Ok(Request {
        method,
        path,
        query,
        body,
    })
}

fn respond_json(stream: &mut UnixStream, status: u16, payload: &Value) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let body = payload.to_string();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

fn open_stream() -> Option<UnixStream> {
    let socket_path = riffd_socket_file();
    if !socket_path.exists() {
        return None;
    }
    let stream = UnixStream::connect(&socket_path).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    Some(stream)
}

fn request_json(path: &str) -> Option<Value> {
    let mut stream = open_stream()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: riffd\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let response = String::from_utf8_lossy(&response);
    let (_, body) = response.split_once("\r\n\r\n")?;
    serde_json::from_str(body).ok()
}

/// Identity of whatever answers the socket, if anything does.
pub(crate) fn query_identity() -> Option<Value> {
    request_json("/identity")
        .filter(|identity| identity.get("service").and_then(|v| v.as_str()) == Some("riffd"))
}

/// Identity, but only when the daemon actually owns this `RIFF_ROOT`.
pub(crate) fn identity_for_current_root() -> Option<Value> {
    query_identity().filter(|identity| {
        identity.get("riff_root").and_then(|v| v.as_str())
            == Some(normalized_path(&root_dir()).as_str())
    })
}

fn query_health() -> Option<Value> {
    identity_for_current_root()?;
    request_json("/health")
}

/// Follow the bus through the daemon's subscribe stream. Returns `true` only
/// when the caller should not fall back to file tailing (i.e. never — a
/// dropped stream returns `false` so `riff watch` degrades to the file).
pub(crate) fn follow_via_daemon(
    cli: &Cli,
    filters: &EventFilters,
    color: bool,
    out: &mut impl Write,
) -> bool {
    if identity_for_current_root().is_none() {
        return false;
    }
    let Some(mut stream) = open_stream() else {
        return false;
    };
    // The subscription blocks indefinitely between events.
    let _ = stream.set_read_timeout(None);
    let request = "GET /subscribe HTTP/1.1\r\nHost: riffd\r\nConnection: close\r\n\r\n";
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    print_verbose(cli, "following via riffd subscribe stream");

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut past_headers = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return false, // daemon went away; fall back
            Ok(_) => {}
        }
        let trimmed = line.trim_end();
        if !past_headers {
            if trimmed.is_empty() {
                past_headers = true;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if filters.matches(&value, trimmed) {
            crate::watch::render_line(out, cli, &value, trimmed, color);
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_event_type(event_type: &str) -> Result<(), String> {
    validate_identifier(event_type, "event type")
}

fn validate_source(source: &str) -> Result<(), String> {
    validate_identifier(source, "source")
}

fn validate_identifier(value: &str, what: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 64 {
        return Err(format!("Invalid {what}: must be 1-64 characters."));
    }
    if !value
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
    {
        return Err(format!(
            "Invalid {what} '{value}': must start with a letter."
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(format!(
            "Invalid {what} '{value}': use letters, digits, '_', '-', '.'."
        ));
    }
    Ok(())
}

fn validate_level(level: &str) -> Result<&'static str, String> {
    match level {
        "info" => Ok(events::LEVEL_INFO),
        "warn" => Ok(events::LEVEL_WARN),
        "error" => Ok(events::LEVEL_ERROR),
        other => Err(format!(
            "Invalid level '{other}': use info, warn, or error."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{filters_from_query, ingest_external_event, validate_identifier, validate_level};

    #[test]
    fn identifiers_are_validated() {
        assert!(validate_identifier("build_finished", "event type").is_ok());
        assert!(validate_identifier("ci.pipeline-2", "event type").is_ok());
        assert!(validate_identifier("", "event type").is_err());
        assert!(validate_identifier("2fast", "event type").is_err());
        assert!(validate_identifier("has space", "event type").is_err());
        assert!(validate_identifier(&"x".repeat(65), "event type").is_err());
    }

    #[test]
    fn levels_are_validated() {
        assert_eq!(validate_level("info"), Ok("info"));
        assert!(validate_level("fatal").is_err());
    }

    #[test]
    fn external_events_require_a_valid_type() {
        assert!(ingest_external_event(b"not json").is_err());
        assert!(ingest_external_event(b"{}").is_err());
        assert!(ingest_external_event(br#"{"type":"has space"}"#).is_err());
        assert!(ingest_external_event(br#"{"type":"ok_event","payload":"str"}"#).is_err());
        // A valid body is accepted even with the bus uninitialized in tests —
        // the emit itself is then a silent no-op by design.
        assert!(ingest_external_event(br#"{"type":"ok_event","source":"ci"}"#).is_ok());
    }

    #[test]
    fn subscribe_query_maps_to_filters() {
        let query = vec![
            ("type".to_string(), "a,b".to_string()),
            ("command".to_string(), "stop".to_string()),
            ("session".to_string(), "s1".to_string()),
        ];
        let filters = filters_from_query(&query);
        assert_eq!(filters.types, vec!["a", "b"]);
        assert_eq!(filters.commands, vec!["stop"]);
        assert_eq!(filters.session.as_deref(), Some("s1"));
    }
}
