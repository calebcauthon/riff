use assert_cmd::Command;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn cmd_with_root(root: &Path) -> Command {
    let mut cmd = Command::cargo_bin("dictate").expect("dictate binary should build");
    cmd.env("ISPY_ROOT", root);
    cmd.env("ISPY_BEEP", "0");
    cmd
}

fn make_session(root: &Path, session_id: &str, note_md: &str) {
    let session_dir = root.join("sessions").join(session_id);
    fs::create_dir_all(&session_dir).expect("create session dir");
    fs::write(session_dir.join("note.md"), note_md).expect("write note.md");
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
        "start   Start dictation session",
        "shot    Capture screenshot into active session",
        "stop    Stop dictation and transcribe",
        "list    List recent sessions",
        "show    Show note markdown for a session id",
        "copy    Print transcript for a recent session index",
        "html    Open HTML report for a recent session",
        "sounds  Pick start/stop sounds and beep timing",
        "status  Show active session status",
    ];

    for line in must_have {
        assert!(stdout.contains(line), "missing help line: {line}\n{stdout}");
    }

    let order = ["start", "shot", "stop", "list", "show", "copy", "html", "sounds", "status"];
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
fn list_on_empty_root_reports_no_sessions() {
    let td = tempdir().expect("tempdir");

    cmd_with_root(td.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("No sessions found."));
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
        .stderr(predicates::str::contains("Session not found: does-not-exist"));
}

#[test]
fn copy_fails_when_transcript_not_available() {
    let td = tempdir().expect("tempdir");
    make_session(td.path(), "20260413-013012", "# Session\n\nNo transcript here\n");

    cmd_with_root(td.path())
        .arg("copy")
        .assert()
        .failure()
        .code(8)
        .stderr(predicates::str::contains("No transcript found for session"));
}
