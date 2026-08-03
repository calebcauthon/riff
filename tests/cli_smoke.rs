use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

fn cmd_with_root(root: &Path) -> Command {
    let mut cmd = Command::cargo_bin("riff").expect("riff binary should build");
    cmd.env("RIFF_ROOT", root);
    cmd.env("RIFF_CONFIG_JSON_FILE", root.join("test-riff-config.json"));
    cmd.env("RIFF_BEEP", "0");
    cmd.env("RIFF_WEB_SERVER", "0");
    cmd.env("RIFF_PARAKEET_SERVER", "0");
    cmd
}

fn cmd_with_root_and_fake_path(root: &Path, fake_bin: &Path) -> Command {
    let mut cmd = cmd_with_root(root);
    let mut paths = vec![fake_bin.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    let joined = env::join_paths(paths).expect("join PATH");
    cmd.env("PATH", joined);
    cmd
}

fn make_session(root: &Path, session_id: &str, note_md: &str) {
    let session_dir = root.join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("create session dir");
    fs::write(session_dir.join("note.md"), note_md).expect("write note.md");
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).expect("write script");
    let mut perm = fs::metadata(path).expect("metadata").permissions();
    perm.set_mode(0o755);
    fs::set_permissions(path, perm).expect("chmod +x");
}

fn install_fake_tools(dir: &Path) {
    fs::create_dir_all(dir).expect("create fake tools dir");

    write_executable(
        &dir.join("ffmpeg"),
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"-list_devices true"* ]]; then
  echo "AVFoundation audio devices"
  echo "[0] Built-in Microphone"
  exit 0
fi
out="${@: -1}"
mkdir -p "$(dirname "$out")"
: > "$out"
trap 'exit 0' INT TERM
while true; do sleep 1; done
"#,
    );

    write_executable(
        &dir.join("screencapture"),
        r#"#!/usr/bin/env bash
set -euo pipefail
out="${@: -1}"
mkdir -p "$(dirname "$out")"
printf '%b' '\x89\x50\x4E\x47\x0D\x0A\x1A\x0A\x00\x00\x00\x0D\x49\x48\x44\x52\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1F\x15\xC4\x89\x00\x00\x00\x0A\x49\x44\x41\x54\x78\x9C\x63\x00\x01\x00\x00\x05\x00\x01\x0D\x0A\x2D\xB4\x00\x00\x00\x00\x49\x45\x4E\x44\xAE\x42\x60\x82' > "$out"
exit 0
"#,
    );

    write_executable(
        &dir.join("osascript"),
        r#"#!/usr/bin/env bash
set -euo pipefail
printf 'TestApp\tcom.example.TestApp\t4242\tExample Window\n'
exit 0
"#,
    );

    write_executable(
        &dir.join("pbcopy"),
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ -n "${RIFF_TEST_PBCOPY_OUT:-}" ]]; then
  cat >"$RIFF_TEST_PBCOPY_OUT"
else
  cat >/dev/null
fi
exit 0
"#,
    );

    write_executable(
        &dir.join("ps"),
        r#"#!/usr/bin/env bash
set -euo pipefail
printf '12.3 4.5 67890 01:23 R /Applications/TestApp.app/Contents/MacOS/TestApp --demo\n'
exit 0
"#,
    );

    write_executable(
        &dir.join("afplay"),
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ -n "${RIFF_TEST_AFPLAY_OUT:-}" ]]; then
  printf 'afplay %s\n' "$*" >> "$RIFF_TEST_AFPLAY_OUT"
fi
exit 0
"#,
    );
}

fn install_fake_open(dir: &Path) {
    fs::create_dir_all(dir).expect("create fake tools dir");
    write_executable(
        &dir.join("open"),
        r#"#!/usr/bin/env bash
set -euo pipefail
exit 0
"#,
    );
}

fn only_session_id(root: &Path) -> String {
    let sessions_dir = root.join("sessions");
    let entries = fs::read_dir(&sessions_dir)
        .expect("read sessions dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect::<Vec<PathBuf>>();

    assert_eq!(entries.len(), 1, "expected exactly 1 session dir");

    entries[0]
        .file_name()
        .and_then(|n| n.to_str())
        .expect("session id")
        .to_string()
}

fn active_session_id(root: &Path) -> String {
    let raw = fs::read_to_string(root.join("active_session.json")).expect("read active session");
    let parsed: Value = serde_json::from_str(&raw).expect("parse active session json");
    parsed
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("active session id")
        .to_string()
}

fn extract_transcript_section(note_markdown: &str) -> String {
    let marker = "## Transcript";
    let start = note_markdown
        .find(marker)
        .expect("note should contain transcript section")
        + marker.len();
    let after = note_markdown[start..].trim_start_matches('\n');
    let end = after.find("\n## ").unwrap_or(after.len());
    after[..end].to_string()
}

#[test]
fn help_lists_commands_in_logical_order_with_descriptions() {
    let td = tempdir().expect("tempdir");

    let out = cmd_with_root(td.path())
        .arg("--help")
        .output()
        .expect("run --help");

    assert!(out.status.success(), "help should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);

    let must_have = [
        ("start", "Start dictation session"),
        ("shot", "Capture screenshot into active session"),
        ("stop", "Stop dictation and transcribe"),
        (
            "toggle",
            "Toggle dictation session (start if idle, stop if active)",
        ),
        (
            "fork",
            "Split session: stop current recording and immediately start a new one",
        ),
        ("live", "Show running live session status"),
        (
            "chunk",
            "Transcribe audio captured so far and keep recording",
        ),
        (
            "pause",
            "Pause transcription capture while continuing to record audio",
        ),
        ("unpause", "Resume transcription capture after pause"),
        (
            "toggle-pause",
            "Toggle transcription pause state (pause if listening, unpause if paused)",
        ),
        (
            "setup",
            "Provision riff's private transcription environment",
        ),
        (
            "doctor",
            "Check installation, transcription, permissions, and helper health",
        ),
        ("list", "List recent sessions"),
        ("show", "Show note markdown for a session id"),
        (
            "copy",
            "Print session transcript, clipboard, and base64 images to stdout",
        ),
        ("send", "Copy transcript and paste into focused app"),
        ("html", "Open HTML report for a session id"),
        (
            "screenshot-use",
            "Set which derived image is used at the transcript screenshot path",
        ),
        ("sounds", "Pick start/stop sounds and beep timing"),
        (
            "silence",
            "Disable beeps globally (writes RIFF_BEEP=0 to rc file)",
        ),
        (
            "loud",
            "Enable beeps globally (writes RIFF_BEEP=1 to rc file)",
        ),
        ("status", "Show active session status"),
        ("perf", "Show startup/shutdown timing summary from perf log"),
        (
            "kill-server",
            "Kill background helper servers (web + parakeet + daemon)",
        ),
        (
            "daemon",
            "Manage the riff daemon: control socket for events in/out",
        ),
        ("emit", "Append an event to the global bus"),
    ];

    for (name, desc) in must_have {
        assert!(
            stdout.contains(&format!("  {name}")),
            "missing command in help: {name}\n{stdout}"
        );
        assert!(
            stdout.contains(desc),
            "missing help description: {name} -> {desc}\n{stdout}"
        );
    }

    let order = [
        "start",
        "shot",
        "stop",
        "toggle",
        "fork",
        "live",
        "chunk",
        "pause",
        "unpause",
        "toggle-pause",
        "setup",
        "doctor",
        "list",
        "show",
        "copy",
        "send",
        "html",
        "screenshot-use",
        "sounds",
        "silence",
        "loud",
        "status",
        "perf",
        "kill-server",
    ];
    let mut last = 0usize;
    for name in order {
        let idx = stdout
            .find(&format!("  {name}"))
            .unwrap_or_else(|| panic!("missing command in help: {name}\n{stdout}"));
        assert!(idx >= last, "command out of order: {name}\n{stdout}");
        last = idx;
    }
}

#[test]
fn silence_and_loud_update_riffrc_beep_setting() {
    let td = tempdir().expect("tempdir");
    let rc_path = td.path().join("riffrc");
    fs::write(
        &rc_path,
        "export RIFF_PARAKEET_MODEL=nvidia/parakeet-tdt-0.6b-v2\nexport RIFF_BEEP=1\n",
    )
    .expect("write initial rc");

    cmd_with_root(td.path())
        .env("RIFF_RC_FILE", &rc_path)
        .arg("silence")
        .assert()
        .success();
    let after_silence = fs::read_to_string(&rc_path).expect("read rc after silence");
    assert!(
        after_silence.contains("export RIFF_BEEP=0"),
        "silence should set RIFF_BEEP=0:\n{after_silence}"
    );
    assert_eq!(
        after_silence
            .lines()
            .filter(|l| l.trim_start().starts_with("export RIFF_BEEP="))
            .count(),
        1,
        "silence should keep exactly one RIFF_BEEP line:\n{after_silence}"
    );

    cmd_with_root(td.path())
        .env("RIFF_RC_FILE", &rc_path)
        .arg("loud")
        .assert()
        .success();
    let after_loud = fs::read_to_string(&rc_path).expect("read rc after loud");
    assert!(
        after_loud.contains("export RIFF_BEEP=1"),
        "loud should set RIFF_BEEP=1:\n{after_loud}"
    );
    assert_eq!(
        after_loud
            .lines()
            .filter(|l| l.trim_start().starts_with("export RIFF_BEEP="))
            .count(),
        1,
        "loud should keep exactly one RIFF_BEEP line:\n{after_loud}"
    );
}

#[test]
fn version_flag_reads_repo_version_file() {
    let td = tempdir().expect("tempdir");
    let expected_version =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("VERSION"))
            .expect("read VERSION file")
            .trim()
            .to_string();

    let out = cmd_with_root(td.path())
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(out.status.success(), "--version should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("riff {expected_version}")),
        "unexpected --version output: {stdout}"
    );
}

#[test]
fn doctor_resolves_installed_resources_when_run_outside_repo() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);

    let resource_dir = td.path().join("installed").join("libexec");
    let scripts_dir = resource_dir.join("scripts");
    fs::create_dir_all(&scripts_dir).expect("create scripts dir");
    write_executable(
        &scripts_dir.join("parakeet_transcribe.py"),
        "#!/usr/bin/env python3\n",
    );
    write_executable(
        &scripts_dir.join("riff_web_server.py"),
        "#!/usr/bin/env python3\n",
    );
    write_executable(
        &scripts_dir.join("pick_riff_sounds.sh"),
        "#!/usr/bin/env bash\n",
    );
    fs::write(
        scripts_dir.join("parakeet-requirements.txt"),
        "nemo_toolkit[asr]==2.4.0\ntorch==2.7.1\nsoundfile==0.13.1\n",
    )
    .expect("write requirements");

    let runtime_dir = td.path().join("runtime").join("python");
    fs::create_dir_all(runtime_dir.join("bin")).expect("create runtime bin");
    write_executable(
        &runtime_dir.join("bin").join("python"),
        "#!/usr/bin/env bash\nexit 0\n",
    );

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .current_dir(env::temp_dir())
        .env("RIFF_RESOURCE_DIR", &resource_dir)
        .env("RIFF_RUNTIME_DIR", &runtime_dir)
        .arg("doctor")
        .assert()
        .success()
        .stdout(
            predicates::str::contains("parakeet_script").and(predicates::str::contains(
                scripts_dir
                    .join("parakeet_transcribe.py")
                    .display()
                    .to_string(),
            )),
        );
}

#[test]
fn toggle_starts_when_idle_and_stops_when_active() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);

    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "toggle",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    cmd_with_root(td.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("Active session:"));

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "toggle",
            "--transcribe-cmd",
            "printf 'toggle test\\n' > {out_txt}",
        ])
        .assert()
        .success();

    cmd_with_root(td.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("No active session."));
}

#[test]
fn fork_splits_session_and_keeps_new_session_active() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);

    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    let first_session = active_session_id(td.path());

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_TRANSCRIBE_CMD", "printf 'fork test\\n' > {out_txt}")
        .arg("fork")
        .assert()
        .success();

    let second_session = active_session_id(td.path());
    assert_ne!(
        first_session, second_session,
        "fork should rotate session id"
    );

    assert!(
        td.path()
            .join("sessions")
            .join(&first_session)
            .join("note.md")
            .exists(),
        "fork should finalize old session note"
    );
    assert!(
        td.path()
            .join("sessions")
            .join(&second_session)
            .join("audio.wav")
            .exists(),
        "fork should have active recording for new session"
    );
}

#[test]
fn list_on_empty_root_reports_no_sessions() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("No sessions found."));
}

#[test]
fn perf_reports_no_records_when_empty() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path())
        .arg("perf")
        .assert()
        .success()
        .stdout(predicates::str::contains("No perf records found."));
}

#[test]
fn perf_json_reports_parakeet_startup_separately() {
    let td = tempdir().expect("tempdir");
    fs::write(
        td.path().join("perf.jsonl"),
        concat!(
            "{\"action\":\"start\",\"total_ms\":100.0}\n",
            "{\"action\":\"parakeet_server_startup\",\"status\":\"ready\",\"total_ms\":8000.0}\n",
            "{\"action\":\"parakeet_server_startup\",\"status\":\"ready\",\"total_ms\":10000.0}\n",
            "{\"action\":\"parakeet_server_startup\",\"status\":\"error\",\"total_ms\":5000.0}\n"
        ),
    )
    .expect("write perf log");

    cmd_with_root(td.path())
        .arg("perf")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "parakeet cold start: count=2 avg=9000.0ms p50=10000.0ms p95=10000.0ms errors=1",
        ));

    let out = cmd_with_root(td.path())
        .args(["--json", "--quiet", "perf"])
        .output()
        .expect("run perf --json");
    assert!(out.status.success());
    let payload: Value = serde_json::from_slice(&out.stdout).expect("parse perf json");
    let summary = &payload["summary"]["parakeet_server_startup"];
    assert_eq!(summary["count"].as_u64(), Some(2));
    assert_eq!(summary["avg_ms"].as_f64(), Some(9000.0));
    assert_eq!(summary["p50_ms"].as_f64(), Some(10000.0));
    assert_eq!(summary["p95_ms"].as_f64(), Some(10000.0));
    assert_eq!(summary["error_count"].as_u64(), Some(1));
    assert_eq!(payload["summary"]["start"]["avg_ms"].as_f64(), Some(100.0));
}

#[test]
fn start_with_healthy_parakeet_reports_no_cold_start() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");
    let marker = td.path().join("python-invoked");
    let parakeet_script = td.path().join("fake_parakeet.py");
    fs::write(&parakeet_script, "# fake\n").expect("write fake parakeet script");
    let canonical_root = fs::canonicalize(td.path()).expect("canonicalize temp root");
    let canonical_script = fs::canonicalize(&parakeet_script).expect("canonicalize fake script");
    let health = json!({
        "ok": true,
        "protocol_version": 1,
        "service": "parakeet",
        "server_instance_id": "healthy-test-instance",
        "pid": 123,
        "riff_root": canonical_root.display().to_string(),
        "script_path": canonical_script.display().to_string(),
        "transport": "unix",
        "endpoint": format!("unix://{}", canonical_root.join("parakeet-server.sock").display()),
        "model": "nvidia/stt_en_fastconformer_hybrid_medium_streaming_80ms_pc",
        "model_revision": "main",
        "requested_device": "auto",
        "device": "cpu",
        "python_version": "3.12.0",
        "python_executable": fake_bin.join("python3").display().to_string(),
        "nemo_version": "2.4.0",
        "torch_version": "2.7.1",
        "started_at_epoch": 1.0
    });
    write_executable(
        &fake_bin.join("curl"),
        &format!("#!/usr/bin/env bash\nprintf '%s\\n' '{}'\n", health),
    );
    write_executable(
        &fake_bin.join("python3"),
        &format!(
            "#!/usr/bin/env bash\nprintf invoked > '{}'\n",
            marker.display()
        ),
    );

    let out = cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_PARAKEET_SERVER", "1")
        .env("RIFF_CLIPBOARD_MONITOR", "0")
        .env("RIFF_PYTHON_BIN", fake_bin.join("python3"))
        .env("RIFF_PARAKEET_SCRIPT", &parakeet_script)
        .env(
            "RIFF_PARAKEET_MODEL",
            "nvidia/stt_en_fastconformer_hybrid_medium_streaming_80ms_pc",
        )
        .args([
            "--json",
            "--quiet",
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .output()
        .expect("run start");
    assert!(out.status.success());
    let payload: Value = serde_json::from_slice(&out.stdout).expect("parse start json");
    assert_eq!(
        payload["parakeet_server_warmup"]["outcome"].as_str(),
        Some("already_healthy")
    );
    assert!(!marker.exists(), "healthy server should not spawn Python");
    let perf = fs::read_to_string(td.path().join("perf.jsonl")).expect("read perf log");
    assert_eq!(
        perf.lines()
            .filter(|line| line.contains("parakeet_server_startup"))
            .count(),
        0
    );

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args(["stop", "--transcribe-cmd", "printf '' > {out_txt}"])
        .assert()
        .success();
}

#[test]
fn cold_start_returns_immediately_and_correlates_readiness_event() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");
    let parakeet_script = td.path().join("fake_parakeet.py");
    fs::write(&parakeet_script, "# fake\n").expect("write fake parakeet script");
    write_executable(&fake_bin.join("curl"), "#!/usr/bin/env bash\nexit 22\n");
    write_executable(
        &fake_bin.join("python3"),
        r#"#!/usr/bin/env bash
set -euo pipefail
shift
instance=""
session=""
action=""
perf_log=""
model=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --startup-instance-id) instance="$2"; shift 2 ;;
    --startup-trigger-session-id) session="$2"; shift 2 ;;
    --startup-trigger-action) action="$2"; shift 2 ;;
    --startup-perf-log) perf_log="$2"; shift 2 ;;
    --model) model="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '{"action":"parakeet_server_startup","status":"ready","instance_id":"%s","trigger_session_id":"%s","trigger_action":"%s","pid":%s,"model":"%s","device":"cpu","total_ms":1.0,"phases":{"python_bootstrap_ms":1.0}}\n' "$instance" "$session" "$action" "$$" "$model" >> "$perf_log"
trap 'exit 0' TERM INT
while true; do sleep 1; done
"#,
    );

    let started = std::time::Instant::now();
    let out = cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_PARAKEET_SERVER", "1")
        .env("RIFF_CLIPBOARD_MONITOR", "0")
        .env("RIFF_PYTHON_BIN", fake_bin.join("python3"))
        .env("RIFF_PARAKEET_SCRIPT", &parakeet_script)
        .args([
            "--json",
            "--quiet",
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .output()
        .expect("run start");
    assert!(out.status.success());
    assert!(started.elapsed() < Duration::from_secs(2));
    let payload: Value = serde_json::from_slice(&out.stdout).expect("parse start json");
    let warmup = &payload["parakeet_server_warmup"];
    assert_eq!(warmup["outcome"].as_str(), Some("spawned"));
    let instance_id = warmup["instance_id"].as_str().expect("instance id");
    let session_id = payload["session_id"].as_str().expect("session id");

    let perf_path = td.path().join("perf.jsonl");
    for _ in 0..100 {
        let ready = fs::read_to_string(&perf_path)
            .map(|text| text.contains("parakeet_server_startup"))
            .unwrap_or(false);
        if ready {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let events = fs::read_to_string(&perf_path).expect("read perf log");
    let startup_events = events
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["action"] == "parakeet_server_startup")
        .collect::<Vec<_>>();
    assert_eq!(startup_events.len(), 1);
    assert_eq!(startup_events[0]["instance_id"], instance_id);
    assert_eq!(startup_events[0]["trigger_session_id"], session_id);
    assert_eq!(startup_events[0]["trigger_action"], "start");
    assert_eq!(startup_events[0]["pid"], warmup["pid"]);

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args(["stop", "--transcribe-cmd", "printf '' > {out_txt}"])
        .assert()
        .success();
    cmd_with_root(td.path())
        .arg("kill-server")
        .assert()
        .success();
}

#[test]
fn show_uses_session_id_and_prints_note_markdown() {
    let td = tempdir().expect("tempdir");
    let session_id = "20260413-013011";
    let note = "# Session\n\n## Transcript\nhello world\n";
    make_session(td.path(), session_id, note);

    cmd_with_root(td.path())
        .args(["show", session_id])
        .assert()
        .success()
        .stdout(predicates::str::contains("# Session"))
        .stdout(predicates::str::contains("hello world"));
}

#[test]
fn show_with_missing_session_id_fails_cleanly() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path())
        .args(["show", "does-not-exist"])
        .assert()
        .failure()
        .code(8)
        .stderr(predicates::str::contains(
            "Session not found: does-not-exist",
        ));
}

#[test]
fn copy_fails_when_transcript_not_available() {
    let td = tempdir().expect("tempdir");
    make_session(
        td.path(),
        "20260413-013012",
        "# Session\n\nNo transcript here\n",
    );

    cmd_with_root(td.path())
        .arg("copy")
        .assert()
        .failure()
        .code(8)
        .stderr(predicates::str::contains("No transcript found for session"));
}

#[test]
fn copy_prints_transcript_from_most_recent_session() {
    let td = tempdir().expect("tempdir");
    make_session(
        td.path(),
        "20260413-013011",
        "# Session\n\n## Transcript\nolder words\n",
    );
    make_session(
        td.path(),
        "20260413-013012",
        "# Session\n\n## Transcript\nnew words here\n",
    );

    cmd_with_root(td.path())
        .arg("copy")
        .assert()
        .success()
        .stdout(predicates::str::contains("new words here"))
        .stdout(predicates::str::contains("older words").not());
}

#[test]
fn copy_omits_redundant_annotation_markers_from_transcript_body() {
    let td = tempdir().expect("tempdir");
    make_session(
        td.path(),
        "20260413-013012",
        concat!(
            "# Session\n\n",
            "## Transcript\n",
            "ghostty Screenshot 1: /tmp/riff/sessions/20260413-013012/screenshots/shot-001.png\n",
            "Clipboard 1: \"git add <file>\"\n\n",
            "Testing, testing, testing. I've got a screenshot and copied text. ",
            "[ghostty Screenshot 1] [Clipboard 1]\n",
        ),
    );

    cmd_with_root(td.path())
        .arg("copy")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Testing, testing, testing. I've got a screenshot and copied text.",
        ))
        .stdout(predicates::str::contains("[ghostty Screenshot 1]").not())
        .stdout(predicates::str::contains("[Clipboard 1]").not());
}

#[test]
fn copy_appends_clipboard_captures_and_base64_screenshots() {
    let td = tempdir().expect("tempdir");
    let session_id = "20260413-013012";
    make_session(
        td.path(),
        session_id,
        "# Session\n\n## Transcript\nnew words here\n",
    );
    let session_dir = td.path().join("sessions").join(session_id);
    fs::write(
        session_dir.join("events.jsonl"),
        [
            r#"{"ts":"2026-04-13T01:30:12.000Z","type":"session_started"}"#,
            r#"{"ts":"2026-04-13T01:31:00.000Z","type":"clipboard_copied","id":1,"audioSec":3.2,"text":"line one\nline two"}"#,
        ]
        .join("\n")
            + "\n",
    )
    .expect("write events");
    fs::create_dir_all(session_dir.join("screenshots")).expect("create screenshots");
    // "Man" -> base64 "TWFu"
    fs::write(session_dir.join("screenshots").join("shot-001.png"), b"Man")
        .expect("write screenshot");

    cmd_with_root(td.path())
        .arg("copy")
        .assert()
        .success()
        .stdout(predicates::str::contains("new words here"))
        .stdout(predicates::str::contains("----- Clipboard captures -----"))
        .stdout(predicates::str::contains(
            "Clipboard 1:\nline one\nline two",
        ))
        .stdout(predicates::str::contains(
            "----- Screenshot 1 — shot-001.png (image/png, base64) -----",
        ))
        .stdout(predicates::str::contains("TWFu"));
}

#[test]
fn copy_verbose_prints_frontmatter_and_session_payload() {
    let td = tempdir().expect("tempdir");
    let session_id = "20260413-013012";
    make_session(
        td.path(),
        session_id,
        "# Session\n\n## Transcript\nnew words here\n",
    );
    let session_dir = td.path().join("sessions").join(session_id);
    fs::write(session_dir.join("transcript.txt"), "new words here\n").expect("write transcript");
    fs::write(
        session_dir.join("events.jsonl"),
        [
            r#"{"ts":"2026-04-13T01:30:12.000Z","type":"session_started"}"#,
            r#"{"ts":"2026-04-13T01:31:00.000Z","type":"clipboard_copied","clip_id":1,"audio_sec":3.2,"text":"clipboard text"}"#,
            r#"{"ts":"2026-04-13T01:31:22.000Z","type":"session_stopped","audio_duration_sec":10.5}"#,
        ]
        .join("\n")
            + "\n",
    )
    .expect("write events");
    fs::write(session_dir.join("ffmpeg.log"), "ffmpeg details\n").expect("write ffmpeg log");
    fs::write(session_dir.join("audio.wav"), b"RIFF").expect("write audio placeholder");
    fs::create_dir_all(session_dir.join("screenshots")).expect("create screenshots");
    fs::write(
        session_dir.join("screenshots").join("shot-001.png"),
        b"\x89PNG\r\n\x1a\n",
    )
    .expect("write screenshot");

    cmd_with_root(td.path())
        .args(["copy", "--verbose"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "---\nsession_id: \"20260413-013012\"",
        ))
        .stdout(predicates::str::contains("files:\n  note_md:"))
        .stdout(predicates::str::contains("screenshot_files:\n  - "))
        .stdout(predicates::str::contains("## Transcript"))
        .stdout(predicates::str::contains("new words here"))
        .stdout(predicates::str::contains("## Events JSONL (events.jsonl)"))
        .stdout(predicates::str::contains("\"type\":\"session_started\""));
}

#[test]
fn send_fails_when_transcript_not_available() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    make_session(
        td.path(),
        "20260413-013012",
        "# Session\n\nNo transcript here\n",
    );

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .arg("send")
        .assert()
        .failure()
        .code(8)
        .stderr(predicates::str::contains("No transcript found for session"));
}

#[test]
fn send_copies_and_pastes_transcript_from_most_recent_session() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    make_session(
        td.path(),
        "20260413-013011",
        "# Session\n\n## Transcript\nolder words\n",
    );
    make_session(
        td.path(),
        "20260413-013012",
        "# Session\n\n## Transcript\nnew words here\n",
    );

    let pbcopy_out = td.path().join("pbcopy.out");
    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_TEST_PBCOPY_OUT", &pbcopy_out)
        .arg("send")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Sent transcript from session 20260413-013012 to focused app.",
        ));

    let copied = fs::read_to_string(&pbcopy_out).expect("pbcopy output should exist");
    assert_eq!(copied, "new words here");
}

#[test]
fn html_generates_sessions_index_and_navigation_link() {
    let td = tempdir().expect("tempdir");
    make_session(
        td.path(),
        "20260413-013011",
        "# Session\n\n## Transcript\nolder words\n",
    );
    make_session(
        td.path(),
        "20260413-013012",
        "# Session\n\n## Transcript\nnew words\n",
    );
    let shots_dir = td
        .path()
        .join("sessions")
        .join("20260413-013012")
        .join("screenshots");
    fs::create_dir_all(&shots_dir).expect("create screenshots dir");
    fs::write(shots_dir.join("shot-1.png"), b"fakepng").expect("write shot-1");
    fs::write(shots_dir.join("shot-2.png"), b"fakepng").expect("write shot-2");

    let fake_bin = td.path().join("fake-bin");
    install_fake_open(&fake_bin);

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .arg("html")
        .assert()
        .success();

    let index_path = td.path().join("sessions").join("index.html");
    let index_html = fs::read_to_string(&index_path).expect("sessions index should exist");
    assert!(index_html.contains("./20260413-013012/note.html"));
    assert!(index_html.contains("./20260413-013011/note.html"));
    assert!(index_html.contains("new words"));
    assert!(index_html.contains("./20260413-013012/screenshots/shot-1.png"));
    assert!(index_html.contains("class=\"thumb\""));
    assert!(index_html.contains("class=\"btn tiny copy-row-transcript\""));
    assert!(index_html.contains("data-href=\"./20260413-013012/note.html\""));

    let note_path = td
        .path()
        .join("sessions")
        .join("20260413-013012")
        .join("note.html");
    let note_html = fs::read_to_string(&note_path).expect("note html should exist");
    assert!(note_html.contains("Browse all sessions"));
    assert!(note_html.contains("../index.html"));
}

#[test]
fn status_reports_no_active_session_when_idle() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("No active session."));
}

#[test]
fn stop_reports_no_active_session_when_idle() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path())
        .arg("stop")
        .assert()
        .success()
        .stdout(predicates::str::contains("No active session."));
}

#[test]
fn no_beeps_flag_supersedes_global_beep_env() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");
    let fake_sound = td.path().join("beep.aiff");
    fs::write(&fake_sound, "beep").expect("write fake sound");
    let afplay_log = td.path().join("afplay.log");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_BEEP", "1")
        .env("RIFF_BEEP_START", &fake_sound)
        .env("RIFF_BEEP_STOP", &fake_sound)
        .env("RIFF_TEST_AFPLAY_OUT", &afplay_log)
        .args([
            "--no-beeps",
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_BEEP", "1")
        .env("RIFF_BEEP_START", &fake_sound)
        .env("RIFF_BEEP_STOP", &fake_sound)
        .env("RIFF_TEST_AFPLAY_OUT", &afplay_log)
        .args([
            "--no-beeps",
            "stop",
            "--transcribe-cmd",
            "printf '' > {out_txt}",
        ])
        .assert()
        .success();

    thread::sleep(Duration::from_millis(120));
    let afplay_output = fs::read_to_string(&afplay_log).unwrap_or_default();
    assert!(
        afplay_output.trim().is_empty(),
        "--no-beeps should suppress beeps even when RIFF_BEEP=1; got:\n{afplay_output}"
    );
}

#[test]
fn stop_without_chunking_skips_stop_flush_chunk_event() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    let out = cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "--json",
            "--quiet",
            "stop",
            "--transcribe-cmd",
            "printf 'ok\\n' > {out_txt}",
        ])
        .output()
        .expect("run stop --json");
    assert!(out.status.success(), "stop should succeed");

    let payload: Value = serde_json::from_slice(&out.stdout).expect("parse stop json");
    assert_eq!(
        payload.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "stop with successful transcription should set ok=true: {payload}"
    );
    assert_ne!(
        payload
            .get("transcription")
            .and_then(|v| v.get("method"))
            .and_then(|v| v.as_str()),
        Some("manual_chunked"),
        "stop without chunking should not use manual_chunked path: {payload}"
    );

    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("session_id in stop payload");
    let events_raw = fs::read_to_string(
        td.path()
            .join("sessions")
            .join(session_id)
            .join("events.jsonl"),
    )
    .expect("read session events");
    assert!(
        !events_raw.contains(r#""type":"transcript_chunk""#),
        "stop without chunking should not append transcript_chunk event:\n{events_raw}"
    );
}

#[test]
fn stop_json_reports_failure_when_transcription_not_ok() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    // No --transcribe-cmd: either skipped (no script) or error (bundled script
    // present but deps missing). Either way stop must not report success.
    let out = cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args(["--json", "--quiet", "stop"])
        .output()
        .expect("run stop --json");

    assert!(
        !out.status.success(),
        "stop should exit non-zero when transcription is not ok; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload: Value = serde_json::from_slice(&out.stdout).expect("parse stop json");
    assert_eq!(
        payload.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "stop --json must set ok=false when transcription is not ok: {payload}"
    );
    let status = payload
        .get("transcription")
        .and_then(|v| v.get("status"))
        .and_then(|v| v.as_str());
    assert!(
        matches!(status, Some("skipped") | Some("error") | Some("missing_audio")),
        "expected non-ok transcription status, got {status:?}: {payload}"
    );
}

#[test]
fn stop_json_reports_failure_when_transcription_errors() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    let out = cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "--json",
            "--quiet",
            "stop",
            "--transcribe-cmd",
            "echo boom >&2; exit 42",
        ])
        .output()
        .expect("run stop --json");

    assert!(
        !out.status.success(),
        "stop should exit non-zero when transcription fails; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload: Value = serde_json::from_slice(&out.stdout).expect("parse stop json");
    assert_eq!(
        payload.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "stop --json must set ok=false when transcription errors: {payload}"
    );
    assert_eq!(
        payload
            .get("transcription")
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_str()),
        Some("error"),
        "expected error transcription status: {payload}"
    );
    assert_eq!(
        payload
            .get("transcription")
            .and_then(|v| v.get("returncode"))
            .and_then(|v| v.as_i64()),
        Some(42),
        "expected transcription returncode in stop json: {payload}"
    );
}

#[test]
fn status_reports_active_session_after_start() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    cmd_with_root(td.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("Active session:"))
        .stdout(predicates::str::contains("alive=true"));

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args(["stop", "--transcribe-cmd", "printf '' > {out_txt}"])
        .assert()
        .success();
}

#[test]
fn stop_json_includes_transcription_perf_breakdown() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    let out = cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "--json",
            "--quiet",
            "stop",
            "--transcribe-cmd",
            "printf 'perf test\\n' > {out_txt}",
        ])
        .output()
        .expect("run stop --json");

    assert!(out.status.success(), "stop should succeed");
    let payload: Value =
        serde_json::from_slice(&out.stdout).expect("stop --json should return valid json payload");

    assert_eq!(
        payload.get("action").and_then(|v| v.as_str()),
        Some("stop"),
        "unexpected stop payload: {payload}"
    );
    assert!(
        payload
            .get("transcription")
            .and_then(|v| v.get("perf"))
            .and_then(|v| v.get("total_ms"))
            .and_then(|v| v.as_f64())
            .is_some(),
        "missing transcription perf total_ms in stop json: {payload}"
    );
    assert_eq!(
        payload
            .get("transcription")
            .and_then(|v| v.get("perf"))
            .and_then(|v| v.get("execution_path"))
            .and_then(|v| v.as_str()),
        Some("custom_command"),
        "unexpected execution_path in stop json: {payload}"
    );

    let perf_log = fs::read_to_string(td.path().join("perf.jsonl")).expect("read perf log");
    let last_stop = perf_log
        .lines()
        .rev()
        .find_map(|line| {
            let parsed: Value = serde_json::from_str(line).ok()?;
            if parsed.get("action").and_then(|v| v.as_str()) == Some("stop") {
                Some(parsed)
            } else {
                None
            }
        })
        .expect("find stop perf record");

    assert!(
        last_stop
            .get("transcription_perf")
            .and_then(|v| v.get("total_ms"))
            .and_then(|v| v.as_f64())
            .is_some(),
        "stop perf log missing transcription_perf.total_ms: {last_stop}"
    );
}

#[test]
fn stop_verbose_prints_hook_instrumentation() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_POST_TRANSCRIBE_CMD", "printf '%s' {transcript}")
        .args([
            "--verbose",
            "stop",
            "--transcribe-cmd",
            "printf 'verbose test\\n' > {out_txt}",
        ])
        .assert()
        .success()
        .stderr(predicates::str::contains("[verbose] Stop pipeline:"))
        .stderr(predicates::str::contains("transcribe_cmd=cli"))
        .stderr(predicates::str::contains("post_transcribe_cmd=env"))
        .stderr(predicates::str::contains("[verbose] Transcription result:"))
        .stderr(predicates::str::contains("[verbose] Post-transcribe hook:"))
        .stderr(predicates::str::contains(
            "[verbose] Stop instrumentation summary:",
        ));
}

#[test]
fn stop_no_stop_hooks_disables_stop_hooks() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    let hook_marker = td.path().join("hook-marker.txt");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env(
            "RIFF_POST_TRANSCRIBE_CMD",
            format!(
                "printf 'post-hook-ran\\n' >> {} && printf '%s' {{transcript}}",
                hook_marker.display()
            ),
        )
        .args([
            "--verbose",
            "stop",
            "--no-stop-hooks",
            "--transcribe-cmd",
            &format!(
                "printf 'transcribe-hook-ran\\n' >> {}",
                hook_marker.display()
            ),
        ])
        // --no-stop-hooks clears --transcribe-cmd, so transcription is not ok and
        // stop correctly exits non-zero. Still verify hooks were disabled.
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("no_stop_hooks=true"))
        .stderr(predicates::str::contains("transcribe_cmd=disabled"))
        .stderr(predicates::str::contains("post_transcribe_cmd=disabled"))
        .stderr(predicates::str::contains(
            "[verbose] Post-transcribe hook: status=skipped source=disabled",
        ));

    assert!(
        !hook_marker.exists(),
        "stop hooks should be disabled, but marker file exists"
    );
}

#[test]
fn end_to_end_start_shot_stop_produces_transcript_and_note() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);

    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .arg("shot")
        .assert()
        .success();

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "stop",
            "--transcribe-cmd",
            "printf 'hello from integration test\\n' > {out_txt}",
        ])
        .assert()
        .success();

    let session_id = only_session_id(td.path());
    let session_dir = td.path().join("sessions").join(&session_id);

    let transcript_txt = fs::read_to_string(session_dir.join("transcript.txt"))
        .expect("transcript.txt should exist");
    assert!(
        transcript_txt.contains("hello from integration test"),
        "unexpected transcript.txt: {transcript_txt}"
    );

    let note_md = fs::read_to_string(session_dir.join("note.md")).expect("note.md should exist");
    assert!(
        note_md.contains("hello from integration test"),
        "note.md missing transcript text: {note_md}"
    );
    assert!(
        note_md.contains("[TestApp Screenshot 1]"),
        "note.md missing screenshot marker: {note_md}"
    );
    assert!(
        note_md.contains("App: TestApp"),
        "note.md missing screenshot app metadata: {note_md}"
    );
    assert!(
        note_md.contains("Window: Example Window"),
        "note.md missing screenshot window metadata: {note_md}"
    );
    assert!(
        note_md.contains("## Screenshot Metadata"),
        "note.md missing screenshot metadata section: {note_md}"
    );
    assert!(
        note_md.contains("[Screenshot 1]"),
        "note.md missing per-screenshot metadata header: {note_md}"
    );
    assert!(
        note_md.contains("cpu=12.3%"),
        "note.md missing screenshot cpu metric: {note_md}"
    );
    assert!(
        note_md.contains("mem=4.5%"),
        "note.md missing screenshot memory metric: {note_md}"
    );
    let transcript_section = extract_transcript_section(&note_md);
    let shot_path = session_dir.join("screenshots").join("shot-001.png");
    let expected_prefix = format!("TestApp Screenshot 1: {}\n\n", shot_path.display());
    assert!(
        transcript_section.starts_with(&expected_prefix),
        "transcript should start with screenshot path then two line breaks: {transcript_section}"
    );
    let disallowed_prefix = format!("TestApp Screenshot 1: {}\n\n\n", shot_path.display());
    assert!(
        !transcript_section.starts_with(&disallowed_prefix),
        "transcript should not have more than two line breaks after path: {transcript_section}"
    );

    cmd_with_root(td.path())
        .args(["show", &session_id])
        .assert()
        .success()
        .stdout(predicates::str::contains("hello from integration test"))
        .stdout(predicates::str::contains("[TestApp Screenshot 1]"));
}

#[test]
fn json_config_post_transcribe_command_rewrites_transcript() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);

    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    let config_path = td.path().join("riff.json");
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "riff": {
                "post_transcribe_cmd": "printf 'rewritten: %s\\n' {transcript}"
            }
        }))
        .expect("serialize config"),
    )
    .expect("write config");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_CONFIG_JSON_FILE", &config_path)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_CONFIG_JSON_FILE", &config_path)
        .args([
            "stop",
            "--transcribe-cmd",
            "printf 'hello from raw transcript\\n' > {out_txt}",
        ])
        .assert()
        .success();

    let session_id = only_session_id(td.path());
    let session_dir = td.path().join("sessions").join(&session_id);
    let transcript_txt = fs::read_to_string(session_dir.join("transcript.txt"))
        .expect("transcript.txt should exist");
    assert!(
        transcript_txt.contains("rewritten: hello from raw transcript"),
        "transcript.txt should contain rewritten text: {transcript_txt}"
    );

    let note_md = fs::read_to_string(session_dir.join("note.md")).expect("read note.md");
    assert!(
        note_md.contains("rewritten: hello from raw transcript"),
        "note.md should contain rewritten transcript: {note_md}"
    );
}

#[test]
fn screenshot_use_swaps_transcript_image_and_keeps_original_backup() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .arg("shot")
        .assert()
        .success();

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "stop",
            "--transcribe-cmd",
            "printf 'hello screenshot use\\n' > {out_txt}",
        ])
        .assert()
        .success();

    let session_id = only_session_id(td.path());
    let session_dir = td.path().join("sessions").join(&session_id);
    let transcript_path = session_dir.join("screenshots").join("shot-001.png");
    let before = fs::read(&transcript_path).expect("read original transcript image");
    let polaroid_path = session_dir
        .join("screenshots")
        .join("derived")
        .join("shot-001__polaroid.png");
    let polaroid_before = fs::read(&polaroid_path).expect("read derived polaroid before use");

    cmd_with_root(td.path())
        .args([
            "screenshot-use",
            "--session-id",
            &session_id,
            "--shot-id",
            "1",
            "--module",
            "polaroid",
        ])
        .assert()
        .success();

    let after = fs::read(&transcript_path).expect("read swapped transcript image");
    let backup_path = session_dir
        .join("screenshots")
        .join("shot-001__original.png");
    let backup = fs::read(&backup_path).expect("read original backup image");
    let polaroid_after = fs::read(&polaroid_path).expect("read derived polaroid after use");

    assert_ne!(before, after, "transcript screenshot should be replaced");
    assert_eq!(before, backup, "backup should keep original image bytes");
    assert_eq!(
        after, polaroid_before,
        "transcript screenshot should be a byte-for-byte copy of selected variant"
    );
    assert_eq!(
        polaroid_before, polaroid_after,
        "derived variant bytes should not be rewritten after selecting transcript image"
    );
}

// ---------------------------------------------------------------------------
// Global event bus + `riff watch`
// ---------------------------------------------------------------------------

fn read_bus(root: &Path) -> Vec<Value> {
    let text = fs::read_to_string(root.join("events.jsonl")).unwrap_or_default();
    text.lines()
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|e| panic!("bus line is not valid JSON ({e}): {line}"))
        })
        .collect()
}

fn bus_types(events: &[Value], command: &str) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["command"] == json!(command))
        .map(|e| e["type"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn every_invocation_emits_command_lifecycle_events() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path()).arg("list").assert().success();
    cmd_with_root(td.path()).arg("hooks").assert().success();
    cmd_with_root(td.path()).arg("status").assert().success();

    let events = read_bus(td.path());
    for command in ["list", "hooks", "status"] {
        let types = bus_types(&events, command);
        assert!(
            types.contains(&"command_started".to_string()),
            "{command} missing command_started, got {types:?}"
        );
        assert!(
            types.contains(&"command_finished".to_string()),
            "{command} missing command_finished, got {types:?}"
        );
    }

    // Every record carries the full envelope.
    for event in &events {
        for key in ["v", "ts", "seq", "inv", "pid", "command", "type", "level"] {
            assert!(
                event.get(key).is_some(),
                "event missing envelope key '{key}': {event}"
            );
        }
        assert_eq!(event["v"], json!(1));
    }
}

#[test]
fn failing_command_emits_command_failed_with_exit_code() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path())
        .args(["show", "no-such-session"])
        .assert()
        .failure();

    let events = read_bus(td.path());
    let failed = events
        .iter()
        .find(|e| e["type"] == json!("command_failed"))
        .expect("command_failed event");
    assert_eq!(failed["command"], json!("show"));
    assert_eq!(failed["exit_code"], json!(8));
    assert_eq!(failed["level"], json!("error"));
    assert!(failed["error"]
        .as_str()
        .unwrap_or_default()
        .contains("no-such-session"));
}

#[test]
fn session_events_are_mirrored_onto_the_bus_with_session_id() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();
    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "stop",
            "--transcribe-cmd",
            "printf 'bus test\\n' > {out_txt}",
        ])
        .assert()
        .success();

    let events = read_bus(td.path());
    let started = events
        .iter()
        .find(|e| e["type"] == json!("session_started"))
        .expect("session_started mirrored onto bus");
    let session_id = started["session_id"].as_str().expect("session_id");
    assert!(!session_id.is_empty());
    assert_eq!(started["command"], json!("start"));

    let stopped = events
        .iter()
        .find(|e| e["type"] == json!("session_stopped"))
        .expect("session_stopped mirrored onto bus");
    assert_eq!(stopped["session_id"], json!(session_id));

    // Stop's live pipeline stages are visible as they happen.
    let stop_types = bus_types(&events, "stop");
    assert!(stop_types.contains(&"transcription_finished".to_string()));
    assert!(stop_types.contains(&"output_hooks_ran".to_string()));
}

#[test]
fn per_session_events_file_stays_free_of_envelope_fields() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    let sessions = fs::read_dir(td.path().join("sessions")).expect("sessions dir");
    let session_dir = sessions
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("one session dir");
    let text = fs::read_to_string(session_dir.join("events.jsonl")).expect("session events");
    assert!(!text.is_empty());
    for line in text.lines() {
        let value: Value = serde_json::from_str(line).expect("session event JSON");
        // Reporting reads this file; the bus envelope must not leak into it.
        for key in ["v", "seq", "inv", "command", "level"] {
            assert!(
                value.get(key).is_none(),
                "session events.jsonl gained envelope key '{key}': {line}"
            );
        }
    }
}

#[test]
fn watch_once_backfills_and_filters() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path()).arg("hooks").assert().success();
    cmd_with_root(td.path()).arg("list").assert().success();

    cmd_with_root(td.path())
        .args(["watch", "--once", "--all"])
        .assert()
        .success()
        .stdout(predicates::str::contains("hooks").and(predicates::str::contains("list")));

    cmd_with_root(td.path())
        .args(["watch", "--once", "--all", "--command", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("list").and(predicates::str::contains("hooks").not()));

    cmd_with_root(td.path())
        .args(["watch", "--once", "--all", "--type", "command_failed"])
        .assert()
        .success()
        .stdout(predicates::str::is_empty());
}

#[test]
fn watch_json_emits_one_json_object_per_line() {
    let td = tempdir().expect("tempdir");
    cmd_with_root(td.path()).arg("hooks").assert().success();

    let output = cmd_with_root(td.path())
        .args(["--json", "watch", "--once", "--all"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8 stdout");
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty());
    for line in lines {
        let value: Value = serde_json::from_str(line).expect("NDJSON line");
        assert!(value.get("type").is_some());
    }
}

#[test]
fn watch_tail_limits_backfill_to_last_n() {
    let td = tempdir().expect("tempdir");
    cmd_with_root(td.path()).arg("hooks").assert().success();
    cmd_with_root(td.path()).arg("list").assert().success();

    let output = cmd_with_root(td.path())
        .args(["--json", "watch", "--once", "-n", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8 stdout");
    assert_eq!(text.lines().filter(|l| !l.trim().is_empty()).count(), 1);
}

#[test]
fn watch_rejects_an_unparseable_since_value() {
    let td = tempdir().expect("tempdir");
    cmd_with_root(td.path())
        .args(["watch", "--once", "--since", "whenever"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Invalid --since"));
}

#[test]
fn bus_rotates_once_it_passes_the_size_cap() {
    let td = tempdir().expect("tempdir");

    for _ in 0..6 {
        cmd_with_root(td.path())
            .env("RIFF_EVENT_BUS_MAX_BYTES", "400")
            .arg("hooks")
            .assert()
            .success();
    }

    assert!(
        td.path().join("events.jsonl.1").exists(),
        "expected a rotated bus generation"
    );
    let live = fs::metadata(td.path().join("events.jsonl"))
        .expect("live bus")
        .len();
    assert!(
        live < 2000,
        "live bus should have restarted small, got {live}"
    );

    // The rotated generation is still visible to a backfill.
    cmd_with_root(td.path())
        .args(["watch", "--once", "--all"])
        .assert()
        .success()
        .stdout(predicates::str::contains("hooks"));
}

#[test]
fn event_bus_can_be_disabled() {
    let td = tempdir().expect("tempdir");
    cmd_with_root(td.path())
        .env("RIFF_EVENT_BUS", "0")
        .arg("hooks")
        .assert()
        .success();
    assert!(
        !td.path().join("events.jsonl").exists(),
        "RIFF_EVENT_BUS=0 should suppress bus writes"
    );
}

#[test]
fn bus_clips_long_payload_strings() {
    let td = tempdir().expect("tempdir");
    let long_id = "z".repeat(500);

    cmd_with_root(td.path())
        .args(["show", &long_id])
        .assert()
        .failure();

    let events = read_bus(td.path());
    let failed = events
        .iter()
        .find(|e| e["type"] == json!("command_failed"))
        .expect("command_failed event");
    let message = failed["error"].as_str().expect("error message");
    assert!(
        message.chars().count() <= 201,
        "error not clipped: {message}"
    );
    assert_eq!(failed["truncated"], json!(true));
}

// ---------------------------------------------------------------------------
// `riff restart` and stray helper discovery
// ---------------------------------------------------------------------------

/// Spawn a process whose command line is indistinguishable from a real
/// riff-owned Parakeet server, without recording a pid file — i.e. a stray.
///
/// `script_dir` must live outside every RIFF_ROOT involved: ownership is
/// decided by whether the argv mentions a root path, and the script path is
/// part of the argv.
fn spawn_fake_stray(script_dir: &Path, owning_root: &Path) -> std::process::Child {
    fs::create_dir_all(script_dir).expect("create scripts dir");
    let script = script_dir.join("parakeet_transcribe.py");
    fs::write(&script, "import time\ntime.sleep(600)\n").expect("write fake script");

    std::process::Command::new("python3")
        .arg(&script)
        .arg("--serve")
        .arg("--riff-root")
        .arg(owning_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn fake stray")
}

/// Reap the child the moment it exits. The stray is a child of this test
/// process, so without a prompt `wait()` a killed stray lingers as a zombie
/// that both `kill(pid, 0)` and `ps` still report as alive — which riff then
/// reads as `still_running`.
fn reap_in_background(mut child: std::process::Child) {
    thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Best-effort cleanup for strays that should already be dead by test end.
fn kill_pid_best_effort(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output();
}

fn pid_is_alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim_start().starts_with(&pid.to_string()))
        })
        .unwrap_or(false)
}

#[test]
fn restart_stops_untracked_strays_that_kill_server_cannot_see() {
    let td = tempdir().expect("tempdir");
    let scripts_td = tempdir().expect("scripts tempdir");
    let stray = spawn_fake_stray(scripts_td.path(), td.path());
    let stray_pid = stray.id();
    reap_in_background(stray);
    thread::sleep(Duration::from_millis(400));

    // No pid file exists, so `kill-server` has nothing to act on.
    cmd_with_root(td.path())
        .arg("kill-server")
        .assert()
        .success();
    assert!(
        pid_is_alive(stray_pid),
        "kill-server should not have found an untracked stray"
    );

    cmd_with_root(td.path())
        .args(["restart", "--parakeet"])
        .assert()
        .success()
        .stdout(predicates::str::contains("orphans=1"));

    thread::sleep(Duration::from_millis(200));
    let alive = pid_is_alive(stray_pid);
    kill_pid_best_effort(stray_pid);
    assert!(!alive, "restart should have stopped the stray");
}

#[test]
fn restart_leaves_helpers_owned_by_another_root_alone() {
    let td = tempdir().expect("tempdir");
    let other_root = tempdir().expect("other root");
    let scripts_td = tempdir().expect("scripts tempdir");
    // Looks exactly like a Parakeet server, but belongs to a different RIFF_ROOT.
    let mut foreign = spawn_fake_stray(scripts_td.path(), other_root.path());
    let foreign_pid = foreign.id();
    thread::sleep(Duration::from_millis(400));

    cmd_with_root(td.path())
        .args(["restart", "--parakeet"])
        .assert()
        .success()
        .stdout(predicates::str::contains("orphans=0"));

    thread::sleep(Duration::from_millis(200));
    assert!(
        pid_is_alive(foreign_pid),
        "a helper owned by another RIFF_ROOT must never be killed"
    );
    let _ = foreign.kill();
    let _ = foreign.wait();
}

#[test]
fn restart_dry_run_reports_without_killing() {
    let td = tempdir().expect("tempdir");
    let scripts_td = tempdir().expect("scripts tempdir");
    let mut stray = spawn_fake_stray(scripts_td.path(), td.path());
    let stray_pid = stray.id();
    thread::sleep(Duration::from_millis(400));

    cmd_with_root(td.path())
        .args(["--dry-run", "restart", "--parakeet"])
        .assert()
        .success()
        .stdout(predicates::str::contains("[dry-run]"));

    assert!(pid_is_alive(stray_pid), "dry run must not kill anything");
    let _ = stray.kill();
    let _ = stray.wait();
}

#[test]
fn doctor_reports_strays_and_clears_after_restart() {
    let td = tempdir().expect("tempdir");
    let scripts_td = tempdir().expect("scripts tempdir");
    let stray = spawn_fake_stray(scripts_td.path(), td.path());
    let stray_pid = stray.id();
    reap_in_background(stray);
    thread::sleep(Duration::from_millis(400));

    cmd_with_root(td.path())
        .arg("doctor")
        .assert()
        .stdout(predicates::str::contains("stray_helpers").and(predicates::str::contains("fail")));

    cmd_with_root(td.path())
        .args(["restart", "--parakeet"])
        .assert()
        .success();
    thread::sleep(Duration::from_millis(200));

    cmd_with_root(td.path()).arg("doctor").assert().stdout(
        predicates::str::contains("stray_helpers            ok")
            .or(predicates::str::contains("stray_helpers").and(predicates::str::contains("none"))),
    );
    kill_pid_best_effort(stray_pid);
}

#[test]
fn restart_emits_orphan_and_restart_events_with_a_readable_pid() {
    let td = tempdir().expect("tempdir");
    let scripts_td = tempdir().expect("scripts tempdir");
    let stray = spawn_fake_stray(scripts_td.path(), td.path());
    let stray_pid = stray.id() as i64;
    reap_in_background(stray);
    thread::sleep(Duration::from_millis(400));

    cmd_with_root(td.path())
        .args(["restart", "--parakeet"])
        .assert()
        .success();

    let events = read_bus(td.path());
    let orphan = events
        .iter()
        .find(|e| e["type"] == json!("helper_orphan_detected"))
        .expect("helper_orphan_detected event");
    assert_eq!(orphan["server"], json!("parakeet"));
    assert_eq!(orphan["level"], json!("warn"));
    // The helper's pid must survive the envelope, which owns the `pid` key.
    assert_eq!(orphan["helper_pid"], json!(stray_pid));
    assert_ne!(orphan["pid"], json!(stray_pid));

    assert!(events
        .iter()
        .any(|e| e["type"] == json!("server_restarted")));
    kill_pid_best_effort(stray_pid as u32);
}

#[test]
fn colliding_payload_keys_are_preserved_not_overwritten() {
    let td = tempdir().expect("tempdir");
    let fake_bin = td.path().join("fake-bin");
    install_fake_tools(&fake_bin);
    let screenshot_source = td.path().join("source-shots");
    fs::create_dir_all(&screenshot_source).expect("create screenshot source dir");

    // `transcription_watcher_*` events carry their own top-level `pid`, which
    // would otherwise be shadowed by the envelope's emitting-process pid.
    cmd_with_root_and_fake_path(td.path(), &fake_bin)
        .env("RIFF_LIVE_TRANSCRIBE", "1")
        .args([
            "start",
            "--screenshot-dir",
            screenshot_source.to_str().expect("path utf8"),
        ])
        .assert()
        .success();

    let events = read_bus(td.path());
    for event in &events {
        // Whatever a payload called `pid`, the envelope's value is this process.
        if let Some(payload_pid) = event.get("payload_pid") {
            assert_ne!(
                payload_pid, &event["pid"],
                "payload pid should have been moved aside, not duplicated"
            );
        }
    }
}

#[test]
fn kill_server_keeps_the_pid_file_when_the_process_survives() {
    let td = tempdir().expect("tempdir");
    // A live process that ignores SIGTERM stands in for a wedged server.
    let mut stubborn = std::process::Command::new("bash")
        .args(["-c", "trap '' TERM; sleep 600"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn stubborn process");
    let pid = stubborn.id();
    fs::create_dir_all(td.path()).ok();
    fs::write(td.path().join("parakeet-server.pid"), pid.to_string()).expect("write pid file");
    thread::sleep(Duration::from_millis(200));

    cmd_with_root(td.path())
        .arg("kill-server")
        .assert()
        .success();

    // SIGKILL is not trappable, so the process does die and the file is cleared.
    // The guard matters only when the pid genuinely outlives our signals; assert
    // the file tracks reality either way.
    let file_exists = td.path().join("parakeet-server.pid").exists();
    let alive = pid_is_alive(pid);
    assert_eq!(
        file_exists, alive,
        "pid file presence must match whether the process is still running"
    );
    let _ = stubborn.kill();
    let _ = stubborn.wait();
}

// ---------------------------------------------------------------------------
// riffd daemon + `riff emit`
// ---------------------------------------------------------------------------

/// Stops the daemon owned by `root` even when the test panics first — a
/// detached riffd is not a child of the test process and would outlive it.
struct DaemonGuard {
    root: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = cmd_with_root(&self.root)
            .args(["--quiet", "daemon", "stop"])
            .ok();
    }
}

fn start_daemon(root: &Path) -> DaemonGuard {
    cmd_with_root(root)
        .args(["daemon", "start"])
        .assert()
        .success();
    DaemonGuard {
        root: root.to_path_buf(),
    }
}

/// One HTTP request over the daemon socket, whole response returned as text.
fn daemon_http(root: &Path, request: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(root.join("riffd.sock"))
        .expect("connect riffd socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set read timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    String::from_utf8_lossy(&response).to_string()
}

fn daemon_http_body(root: &Path, request: &str) -> Value {
    let response = daemon_http(root, request);
    let (_, body) = response
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header/body split in response: {response}"));
    serde_json::from_str(body).unwrap_or_else(|e| panic!("body is not JSON ({e}): {body}"))
}

#[test]
fn emit_appends_a_fully_enveloped_event_without_the_daemon() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path())
        .args([
            "emit",
            "deploy_started",
            "--data",
            r#"{"env":"prod","attempt":2}"#,
            "--level",
            "warn",
        ])
        .assert()
        .success();

    let events = read_bus(td.path());
    let event = events
        .iter()
        .find(|e| e["type"] == json!("deploy_started"))
        .expect("emitted event on the bus");
    assert_eq!(event["command"], json!("emit"));
    assert_eq!(event["level"], json!("warn"));
    assert_eq!(event["env"], json!("prod"));
    assert_eq!(event["attempt"], json!(2));
    assert_eq!(event["v"], json!(1));
}

#[test]
fn emit_rejects_bad_types_payloads_and_levels() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path())
        .args(["emit", "has space"])
        .assert()
        .failure();
    cmd_with_root(td.path())
        .args(["emit", "ok_type", "--data", "not json"])
        .assert()
        .failure();
    cmd_with_root(td.path())
        .args(["emit", "ok_type", "--data", "[1,2]"])
        .assert()
        .failure();
    cmd_with_root(td.path())
        .args(["emit", "ok_type", "--level", "fatal"])
        .assert()
        .failure();
}

#[test]
fn daemon_serves_identity_health_external_events_and_subscriptions() {
    use std::io::{BufRead, BufReader, Write};

    let td = tempdir().expect("tempdir");
    let guard = start_daemon(td.path());

    // Identity names this root and the riffd service.
    let identity = daemon_http_body(
        td.path(),
        "GET /identity HTTP/1.1\r\nHost: riffd\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(identity["service"], json!("riffd"));
    let canonical_root = fs::canonicalize(td.path())
        .expect("canonicalize root")
        .display()
        .to_string();
    assert_eq!(identity["riff_root"], json!(canonical_root));

    let health = daemon_http_body(
        td.path(),
        "GET /health HTTP/1.1\r\nHost: riffd\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(health["ok"], json!(true));

    // External events get a daemon-assigned envelope.
    let body = r#"{"type":"build_finished","source":"ci","payload":{"job":"tests","ok":true}}"#;
    let accepted = daemon_http_body(
        td.path(),
        &format!(
            "POST /events HTTP/1.1\r\nHost: riffd\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert_eq!(accepted["ok"], json!(true));
    assert_eq!(accepted["command"], json!("external:ci"));

    let bad_body = r#"{"type":"has space"}"#;
    let rejected = daemon_http_body(
        td.path(),
        &format!(
            "POST /events HTTP/1.1\r\nHost: riffd\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bad_body}",
            bad_body.len()
        ),
    );
    assert_eq!(rejected["ok"], json!(false));

    let events = read_bus(td.path());
    let external = events
        .iter()
        .find(|e| e["type"] == json!("build_finished"))
        .expect("external event on the bus");
    assert_eq!(external["command"], json!("external:ci"));
    assert_eq!(external["job"], json!("tests"));
    for key in ["v", "ts", "seq", "inv", "pid", "level"] {
        assert!(
            external.get(key).is_some(),
            "external event missing envelope key '{key}': {external}"
        );
    }

    // A subscriber receives matching events pushed over the socket.
    let mut sub = std::os::unix::net::UnixStream::connect(td.path().join("riffd.sock"))
        .expect("connect subscribe");
    sub.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    sub.write_all(
        b"GET /subscribe?type=deploy_started HTTP/1.1\r\nHost: riffd\r\nConnection: close\r\n\r\n",
    )
    .expect("write subscribe request");
    let mut reader = BufReader::new(sub);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).expect("read header line");
        if line.trim_end().is_empty() {
            break;
        }
    }
    // Let the daemon register the subscription before emitting.
    thread::sleep(Duration::from_millis(400));

    cmd_with_root(td.path())
        .args(["--quiet", "emit", "unrelated_event"])
        .assert()
        .success();
    cmd_with_root(td.path())
        .args([
            "--quiet",
            "emit",
            "deploy_started",
            "--data",
            r#"{"env":"ci"}"#,
        ])
        .assert()
        .success();

    line.clear();
    reader.read_line(&mut line).expect("read pushed event");
    let pushed: Value = serde_json::from_str(line.trim_end())
        .unwrap_or_else(|e| panic!("pushed line is not JSON ({e}): {line}"));
    assert_eq!(
        pushed["type"],
        json!("deploy_started"),
        "subscription filter should have skipped unrelated_event"
    );

    drop(guard); // stops the daemon
    assert!(
        !td.path().join("riffd.sock").exists(),
        "daemon stop should remove the socket"
    );
    cmd_with_root(td.path())
        .args(["daemon", "status"])
        .assert()
        .code(1);
}

#[test]
fn watch_follows_through_the_daemon_subscribe_stream() {
    let td = tempdir().expect("tempdir");
    let guard = start_daemon(td.path());

    let out_path = td.path().join("watch-out.jsonl");
    let out_file = fs::File::create(&out_path).expect("create watch output file");
    let bin = env!("CARGO_BIN_EXE_riff");
    let mut watch = std::process::Command::new(bin)
        .args(["--json", "watch"])
        .env("RIFF_ROOT", td.path())
        .env(
            "RIFF_CONFIG_JSON_FILE",
            td.path().join("test-riff-config.json"),
        )
        .env("RIFF_BEEP", "0")
        .env("RIFF_WEB_SERVER", "0")
        .env("RIFF_PARAKEET_SERVER", "0")
        .stdout(out_file)
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn riff watch");

    // Give watch time to connect to the daemon and subscribe.
    thread::sleep(Duration::from_millis(600));
    cmd_with_root(td.path())
        .args(["--quiet", "emit", "external_ping", "--data", r#"{"n":1}"#])
        .assert()
        .success();
    thread::sleep(Duration::from_millis(800));

    let _ = watch.kill();
    let _ = watch.wait();
    drop(guard);

    let seen = fs::read_to_string(&out_path).expect("read watch output");
    assert!(
        seen.lines().any(|l| l.contains("external_ping")),
        "watch should have streamed the emitted event, got: {seen}"
    );
}

#[test]
fn kill_server_also_stops_the_daemon() {
    let td = tempdir().expect("tempdir");
    let _guard = start_daemon(td.path());
    let daemon_pid = fs::read_to_string(td.path().join("riffd.pid"))
        .expect("riffd pid file")
        .trim()
        .parse::<u32>()
        .expect("pid");

    cmd_with_root(td.path())
        .arg("kill-server")
        .assert()
        .success()
        .stdout(predicates::str::contains("riffd"));

    thread::sleep(Duration::from_millis(300));
    assert!(!pid_is_alive(daemon_pid), "riffd should be dead");
    assert!(
        !td.path().join("riffd.sock").exists(),
        "kill-server should remove the daemon socket"
    );
}

#[test]
fn watchdog_emits_mic_listening_once_audio_bytes_flow() {
    let td = tempdir().expect("tempdir");
    let session_id = "20260101-000000";
    let session_dir = td.path().join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("create session dir");
    let audio_path = session_dir.join("audio.wav");
    let events_path = session_dir.join("events.jsonl");

    // A live stand-in for the recorder pid; reaped promptly like the strays.
    let recorder = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn fake recorder");
    let recorder_pid = recorder.id();
    reap_in_background(recorder);

    let started_at_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_secs_f64();
    let bin = env!("CARGO_BIN_EXE_riff");
    let mut watchdog = std::process::Command::new(bin)
        .args([
            "--quiet",
            "watch-max-duration",
            "--session-id",
            session_id,
            "--max-sec",
            "60",
            "--ffmpeg-pid",
            &recorder_pid.to_string(),
            "--started-at-epoch",
            &format!("{started_at_epoch}"),
            "--audio-path",
            audio_path.to_str().expect("utf8"),
            "--events-path",
            events_path.to_str().expect("utf8"),
        ])
        .env("RIFF_ROOT", td.path())
        .env(
            "RIFF_CONFIG_JSON_FILE",
            td.path().join("test-riff-config.json"),
        )
        .env("RIFF_BEEP", "0")
        .spawn()
        .expect("spawn watchdog");

    // Header-only bytes must not count as a listening mic.
    fs::write(&audio_path, vec![0u8; 44]).expect("write wav header");
    thread::sleep(Duration::from_millis(400));
    assert!(
        !fs::read_to_string(&events_path)
            .unwrap_or_default()
            .contains("mic_listening"),
        "44 header bytes must not confirm the mic"
    );

    // Real samples flowing.
    fs::write(&audio_path, vec![0u8; 16_384]).expect("write audio bytes");
    thread::sleep(Duration::from_millis(500));

    kill_pid_best_effort(recorder_pid);
    thread::sleep(Duration::from_millis(300));
    let _ = watchdog.kill();
    let _ = watchdog.wait();

    let session_events = fs::read_to_string(&events_path).expect("session events");
    let mic_line = session_events
        .lines()
        .find(|l| l.contains("mic_listening"))
        .expect("mic_listening in session events");
    let mic: Value = serde_json::from_str(mic_line).expect("mic event json");
    assert_eq!(mic["session_id"], json!(session_id));
    assert!(mic["audio_bytes"].as_u64().expect("audio_bytes") > 4096);
    assert!(mic["confirm_ms"].as_f64().expect("confirm_ms") >= 0.0);
    // The session file keeps its historical flat shape — no envelope keys.
    assert!(mic.get("v").is_none() && mic.get("inv").is_none());

    // And the bus mirror carries the envelope.
    let bus_mic = read_bus(td.path())
        .into_iter()
        .find(|e| e["type"] == json!("mic_listening"))
        .expect("mic_listening on the bus");
    assert_eq!(bus_mic["command"], json!("watch-max-duration"));
    assert_eq!(bus_mic["v"], json!(1));
}
