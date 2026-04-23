use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use serde_json::Value;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn cmd_with_root(root: &Path) -> Command {
    let mut cmd = Command::cargo_bin("riff").expect("riff binary should build");
    cmd.env("RIFF_ROOT", root);
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
        ("list", "List recent sessions"),
        ("show", "Show note markdown for a session id"),
        ("copy", "Print transcript for a recent session index"),
        ("send", "Copy transcript and paste into focused app"),
        ("html", "Open HTML report for a session id"),
        (
            "screenshot-use",
            "Set which derived image is used at the transcript screenshot path",
        ),
        ("sounds", "Pick start/stop sounds and beep timing"),
        ("status", "Show active session status"),
        ("perf", "Show startup/shutdown timing summary from perf log"),
        (
            "kill-server",
            "Kill background helper servers (web + parakeet)",
        ),
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
        "list",
        "show",
        "copy",
        "send",
        "html",
        "screenshot-use",
        "sounds",
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
        .args(["--json", "--quiet", "stop"])
        .output()
        .expect("run stop --json");
    assert!(out.status.success(), "stop should succeed");

    let payload: Value = serde_json::from_slice(&out.stdout).expect("parse stop json");
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
