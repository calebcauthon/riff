//! Helper-server discovery and `riff restart`.
//!
//! `kill-server` only knows the pid recorded in a pid file. A stray helper is
//! precisely the process where that record is gone or wrong — riff crashed
//! between spawn and pid-file write, a kill failed, the socket was removed, or
//! `RIFF_ROOT` moved. This module finds those by scanning the process table
//! instead, and decides ownership from the helper's own argv: every helper is
//! spawned with its owning root on the command line, so a wedged process that
//! answers no health check can still be identified as ours.

use crate::cli::{Cli, RestartArgs};
use crate::error::AppError;
use crate::events;
use crate::paths::{
    ensure_dirs, parakeet_server_pid_file, parakeet_server_socket_file, root_dir,
    web_server_pid_file,
};
use crate::transcription::{
    default_parakeet_script, ensure_parakeet_server, ensure_web_server, normalized_path,
    parakeet_server_enabled, resolve_parakeet_model, resolve_parakeet_script, resolve_python_bin,
    web_server_enabled,
};
use crate::{
    command_exists, emit_json, print_verbose, process_is_alive, read_pid_file, send_signal,
};
use serde_json::{json, Value};
use std::fs;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Helper {
    Parakeet,
    Web,
}

impl Helper {
    fn label(self) -> &'static str {
        match self {
            Helper::Parakeet => "parakeet",
            Helper::Web => "web",
        }
    }

    /// Script basename, matched against the process command line. Riff-specific
    /// enough that combined with the root check it cannot match a stranger.
    fn script_marker(self) -> &'static str {
        match self {
            Helper::Parakeet => "parakeet_transcribe.py",
            Helper::Web => "riff_web_server.py",
        }
    }

    /// Extra token that must be present, to avoid matching a one-shot
    /// transcription or a `--download-model` run as if it were the server.
    fn required_token(self) -> Option<&'static str> {
        match self {
            Helper::Parakeet => Some("--serve"),
            Helper::Web => None,
        }
    }

    fn pid_file(self) -> std::path::PathBuf {
        match self {
            Helper::Parakeet => parakeet_server_pid_file(),
            Helper::Web => web_server_pid_file(),
        }
    }
}

pub(crate) struct HelperProcess {
    pub(crate) pid: i32,
    pub(crate) command: String,
    /// The command line names this `RIFF_ROOT`, so riff may stop it.
    pub(crate) owned: bool,
}

impl HelperProcess {
    fn as_json(&self) -> Value {
        json!({ "pid": self.pid, "owned": self.owned, "command": self.command })
    }
}

/// Every spelling of the current root that could appear in a helper's argv.
/// The Parakeet server is passed a canonicalized root and the web server a raw
/// one, and on macOS `/tmp` canonicalizes to `/private/tmp`, so both count.
fn root_candidates() -> Vec<String> {
    let root = root_dir();
    let mut candidates = vec![root.display().to_string()];
    let normalized = normalized_path(&root);
    if !candidates.contains(&normalized) {
        candidates.push(normalized);
    }
    candidates
}

/// Does this command line represent an actual running helper?
///
/// Deliberately strict, because the result is a kill list. Matching the script
/// name as a substring is not enough: a shell, an editor, a `grep`, or a
/// wrapper script whose arguments merely mention the script would match, and
/// during development that is common. So require all three of:
///
/// 1. argv[0] is a Python interpreter — excludes shells and every other process
///    that only quotes the command,
/// 2. some argument is exactly the script path (or ends with `/<script>`),
/// 3. the mode token (`--serve`) is its own argument, not text inside one.
fn is_helper_invocation(command: &str, helper: Helper) -> bool {
    let mut tokens = command.split_whitespace();

    let Some(argv0) = tokens.next() else {
        return false;
    };
    let interpreter = argv0.rsplit('/').next().unwrap_or(argv0);
    if !interpreter.starts_with("python") && !interpreter.starts_with("Python") {
        return false;
    }

    let rest: Vec<&str> = tokens.collect();
    let marker = helper.script_marker();
    let names_script = rest
        .iter()
        .any(|token| *token == marker || token.ends_with(&format!("/{marker}")));
    if !names_script {
        return false;
    }
    match helper.required_token() {
        Some(required) => rest.iter().any(|token| *token == required),
        None => true,
    }
}

/// Find every running helper of this kind, whoever owns it.
pub(crate) fn scan_helper_processes(helper: Helper) -> Vec<HelperProcess> {
    if !command_exists("ps") {
        return Vec::new();
    }
    let Ok(output) = Command::new("ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let roots = root_candidates();
    let self_pid = std::process::id() as i32;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let split = line.find(char::is_whitespace)?;
            let (pid_text, command) = line.split_at(split);
            let pid: i32 = pid_text.parse().ok()?;
            if pid == self_pid {
                return None;
            }
            let command = command.trim();
            if !is_helper_invocation(command, helper) {
                return None;
            }
            Some(HelperProcess {
                pid,
                owned: roots.iter().any(|root| command.contains(root.as_str())),
                command: command.to_string(),
            })
        })
        .collect()
}

/// Owned helpers the pid file does not account for — the actual strays.
pub(crate) fn stray_helper_pids(helper: Helper) -> Vec<i32> {
    let tracked = read_pid_file(&helper.pid_file());
    scan_helper_processes(helper)
        .into_iter()
        .filter(|p| p.owned && Some(p.pid) != tracked)
        .map(|p| p.pid)
        .collect()
}

pub(crate) fn stop_pid(pid: i32) -> &'static str {
    if !process_is_alive(pid) {
        return "already_exited";
    }
    if send_signal(pid, libc::SIGTERM).is_err() {
        return "signal_failed";
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return "terminated";
        }
        thread::sleep(Duration::from_millis(50));
    }
    if send_signal(pid, libc::SIGKILL).is_err() {
        return "signal_failed";
    }
    thread::sleep(Duration::from_millis(100));
    if process_is_alive(pid) {
        "still_running"
    } else {
        "killed"
    }
}

fn restart_helper(cli: &Cli, helper: Helper, wait_ready: bool) -> Value {
    let found = scan_helper_processes(helper);
    let tracked = read_pid_file(&helper.pid_file());
    let (owned, foreign): (Vec<_>, Vec<_>) = found.into_iter().partition(|p| p.owned);

    for process in &foreign {
        print_verbose(
            cli,
            format!(
                "Leaving {} process pid={} alone: not owned by RIFF_ROOT {}",
                helper.label(),
                process.pid,
                root_dir().display()
            ),
        );
    }
    for process in &owned {
        if Some(process.pid) != tracked {
            events::emit_leveled(
                events::LEVEL_WARN,
                "helper_orphan_detected",
                None,
                json!({
                    "server": helper.label(),
                    "helper_pid": process.pid,
                    "tracked_pid": tracked,
                }),
            );
        }
    }

    if cli.dry_run {
        return json!({
            "server": helper.label(),
            "dry_run": true,
            "found": owned.iter().map(HelperProcess::as_json).collect::<Vec<_>>(),
            "foreign": foreign.iter().map(HelperProcess::as_json).collect::<Vec<_>>(),
        });
    }

    let stopped: Vec<Value> = owned
        .iter()
        .map(|process| {
            let outcome = stop_pid(process.pid);
            json!({ "pid": process.pid, "outcome": outcome })
        })
        .collect();

    // Clear the coordination files only once nothing owned is still running, so
    // a process we failed to kill stays discoverable instead of being forgotten.
    let all_stopped = stopped.iter().all(|s| {
        !matches!(
            s["outcome"].as_str(),
            Some("signal_failed") | Some("still_running")
        )
    });
    if all_stopped {
        let _ = fs::remove_file(helper.pid_file());
        if helper == Helper::Parakeet {
            let _ = fs::remove_file(parakeet_server_socket_file());
        }
    }

    let mut report = json!({
        "server": helper.label(),
        "stopped": stopped,
        "orphans": owned.iter().filter(|p| Some(p.pid) != tracked).count(),
        "foreign": foreign.iter().map(HelperProcess::as_json).collect::<Vec<_>>(),
    });

    let started = if all_stopped {
        start_helper(cli, helper, wait_ready)
    } else {
        json!({ "status": "skipped", "reason": "stop_incomplete" })
    };
    if let Some(map) = report.as_object_mut() {
        map.insert("started".to_string(), started);
    }
    report
}

pub(crate) fn start_helper(cli: &Cli, helper: Helper, wait_ready: bool) -> Value {
    match helper {
        Helper::Parakeet => {
            if !parakeet_server_enabled() {
                return json!({ "status": "disabled" });
            }
            let Some(script_path) = resolve_parakeet_script(None).or_else(default_parakeet_script)
            else {
                return json!({ "status": "error", "reason": "no_parakeet_script" });
            };
            let python_bin = resolve_python_bin(None);
            let model = resolve_parakeet_model(None);
            let warmup = ensure_parakeet_server(
                &python_bin,
                &script_path,
                &model,
                cli,
                wait_ready,
                None,
                "restart",
            );
            warmup.as_json()
        }
        Helper::Web => {
            if !web_server_enabled() {
                return json!({ "status": "disabled" });
            }
            json!({ "status": if ensure_web_server(cli, wait_ready) { "ready" } else { "not_ready" } })
        }
    }
}

pub(crate) fn cmd_restart(cli: &Cli, args: &RestartArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    // Neither flag means both, matching `kill-server`.
    let both = !args.parakeet && !args.web;
    let mut targets = Vec::new();
    if both || args.parakeet {
        targets.push(Helper::Parakeet);
    }
    if both || args.web {
        targets.push(Helper::Web);
    }

    let wait_ready = !args.no_wait;
    let mut reports = Vec::new();
    for helper in targets {
        let report = restart_helper(cli, helper, wait_ready);
        events::emit("server_restarted", None, report.clone());
        reports.push(report);
    }

    if !cli.quiet {
        for report in &reports {
            print_restart_report(report);
        }
    }
    emit_json(
        cli,
        &json!({ "ok": true, "action": "restart", "servers": reports }),
    );
    Ok(0)
}

fn print_restart_report(report: &Value) {
    let server = report["server"].as_str().unwrap_or("unknown");

    if report["dry_run"] == json!(true) {
        let found = report["found"].as_array().map(Vec::len).unwrap_or(0);
        println!("[dry-run] restart {server}: would stop {found} owned process(es)");
        print_foreign(report);
        return;
    }

    let stopped = report["stopped"].as_array().cloned().unwrap_or_default();
    let orphans = report["orphans"].as_u64().unwrap_or(0);
    let outcomes: Vec<String> = stopped
        .iter()
        .map(|s| {
            format!(
                "{}={}",
                s["pid"].as_i64().unwrap_or_default(),
                s["outcome"].as_str().unwrap_or("unknown")
            )
        })
        .collect();
    let started = report["started"]["status"]
        .as_str()
        .or_else(|| report["started"]["outcome"].as_str())
        .unwrap_or("unknown");

    println!(
        "restart {server}: stopped {} ({}) orphans={orphans} started={started}",
        stopped.len(),
        if outcomes.is_empty() {
            "none".to_string()
        } else {
            outcomes.join(" ")
        },
    );
    print_foreign(report);
}

fn print_foreign(report: &Value) {
    let Some(foreign) = report["foreign"].as_array() else {
        return;
    };
    for process in foreign {
        println!(
            "  left alone (other RIFF_ROOT): pid={} {}",
            process["pid"].as_i64().unwrap_or_default(),
            process["command"].as_str().unwrap_or_default()
        );
    }
}

/// Human-readable stray summary for `riff doctor`.
pub(crate) fn stray_summary() -> (bool, String) {
    let parakeet = stray_helper_pids(Helper::Parakeet);
    let web = stray_helper_pids(Helper::Web);
    if parakeet.is_empty() && web.is_empty() {
        return (true, "none".to_string());
    }
    let mut parts = Vec::new();
    if !parakeet.is_empty() {
        parts.push(format!("parakeet {parakeet:?}"));
    }
    if !web.is_empty() {
        parts.push(format!("web {web:?}"));
    }
    (false, format!("{} (run 'riff restart')", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::{is_helper_invocation, Helper};

    const REAL_SERVER: &str = "/Users/x/Code/riff/.venv/bin/python /Users/x/Code/riff/scripts/parakeet_transcribe.py --serve --model nvidia/parakeet --device auto --riff-root /private/tmp/riff --unix-socket /private/tmp/riff/parakeet-server.sock";

    #[test]
    fn matches_a_real_parakeet_server() {
        assert!(is_helper_invocation(REAL_SERVER, Helper::Parakeet));
    }

    #[test]
    fn matches_a_real_web_server() {
        assert!(is_helper_invocation(
            "/usr/bin/python3 /opt/riff/scripts/riff_web_server.py --root /tmp/riff --port 8766",
            Helper::Web
        ));
    }

    // The scan feeds a kill list, so quoting the command must never be enough.
    #[test]
    fn ignores_processes_that_merely_mention_the_script() {
        for command in [
            // A shell running a script whose text contains the command.
            "/bin/bash -c riff restart; python3 scripts/parakeet_transcribe.py --serve --riff-root /tmp/riff",
            "/bin/zsh -c ps -ax | grep parakeet_transcribe.py --serve",
            "grep -r parakeet_transcribe.py --serve /tmp/riff",
            "vim scripts/parakeet_transcribe.py --serve",
            // An editor or tail on the log, not the server itself.
            "tail -f /tmp/riff/parakeet-server.log parakeet_transcribe.py --serve",
        ] {
            assert!(
                !is_helper_invocation(command, Helper::Parakeet),
                "should not match: {command}"
            );
        }
    }

    #[test]
    fn ignores_python_running_a_different_script() {
        assert!(!is_helper_invocation(
            "/usr/bin/python3 /opt/riff/scripts/other_script.py --serve --riff-root /tmp/riff",
            Helper::Parakeet
        ));
        // Substring-adjacent names must not match either.
        assert!(!is_helper_invocation(
            "/usr/bin/python3 /opt/my_parakeet_transcribe.py --serve",
            Helper::Parakeet
        ));
    }

    #[test]
    fn ignores_non_server_modes_of_the_same_script() {
        // One-shot transcription and model downloads are not the server.
        assert!(!is_helper_invocation(
            "/usr/bin/python3 /opt/riff/scripts/parakeet_transcribe.py --audio a.wav --out-txt a.txt",
            Helper::Parakeet
        ));
        assert!(!is_helper_invocation(
            "/usr/bin/python3 /opt/riff/scripts/parakeet_transcribe.py --download-model",
            Helper::Parakeet
        ));
        // The live watcher shares the script but is owned by its session.
        assert!(!is_helper_invocation(
            "/usr/bin/python3 /opt/riff/scripts/parakeet_transcribe.py --watch-audio --session-id 1",
            Helper::Parakeet
        ));
    }

    #[test]
    fn ignores_empty_and_malformed_command_lines() {
        assert!(!is_helper_invocation("", Helper::Parakeet));
        assert!(!is_helper_invocation("python3", Helper::Parakeet));
    }

    #[test]
    fn helper_markers_are_script_specific() {
        assert_eq!(Helper::Parakeet.script_marker(), "parakeet_transcribe.py");
        assert_eq!(Helper::Parakeet.required_token(), Some("--serve"));
        assert_eq!(Helper::Web.script_marker(), "riff_web_server.py");
        assert_eq!(Helper::Web.required_token(), None);
    }
}
