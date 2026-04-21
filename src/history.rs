use crate::cli::{Cli, CopyArgs, ListArgs, PerfArgs, SendArgs, ShowArgs};
use crate::error::{app_error, AppError};
use crate::models::SessionListRow;
use crate::paths::{ensure_dirs, perf_log_file, sessions_dir};
use crate::{command_exists, emit_json, get_audio_duration_sec, print_out, SUPPORTED_IMAGE_EXTS};
use chrono::{DateTime, Datelike, Local, NaiveDateTime, Timelike, Utc};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

pub(crate) fn read_jsonl_values(path: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };

    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

pub(crate) fn session_started_iso(events: &[Value]) -> Option<String> {
    events
        .iter()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("session_started"))
        .and_then(|e| e.get("ts").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

pub(crate) fn session_duration_seconds(events: &[Value], session_dir: &Path) -> Option<f64> {
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

pub(crate) fn extract_transcript_from_note(note_markdown: &str) -> Option<String> {
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

pub(crate) fn read_transcript_text_for_session(session_dir: &Path) -> String {
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

pub(crate) fn format_duration_compact(seconds: Option<f64>) -> String {
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

fn percentile(sorted_values: &[f64], p: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    let clamped = p.clamp(0.0, 1.0);
    let idx = ((sorted_values.len().saturating_sub(1) as f64) * clamped).round() as usize;
    sorted_values[idx]
}

fn compute_total_stats(events: &[Value], action: &str) -> Value {
    let mut totals = events
        .iter()
        .filter(|e| e.get("action").and_then(|v| v.as_str()) == Some(action))
        .filter_map(|e| e.get("total_ms").and_then(|v| v.as_f64()))
        .collect::<Vec<_>>();
    if totals.is_empty() {
        return json!({
            "count": 0,
            "avg_ms": Value::Null,
            "p50_ms": Value::Null,
            "p95_ms": Value::Null
        });
    }
    totals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let count = totals.len();
    let sum = totals.iter().sum::<f64>();
    let avg = sum / (count as f64);
    let p50 = percentile(&totals, 0.50);
    let p95 = percentile(&totals, 0.95);
    json!({
        "count": count,
        "avg_ms": avg,
        "p50_ms": p50,
        "p95_ms": p95
    })
}

fn dominant_phase(event: &Value) -> Option<(String, f64)> {
    let phases = event.get("phases")?.as_object()?;
    let mut best: Option<(String, f64)> = None;
    for (k, v) in phases {
        let Some(ms) = v.as_f64() else {
            continue;
        };
        match &best {
            Some((_, best_ms)) if ms <= *best_ms => {}
            _ => best = Some((k.clone(), ms)),
        }
    }
    best
}

pub(crate) fn cmd_perf(cli: &Cli, args: &PerfArgs) -> Result<i32, AppError> {
    ensure_dirs()?;
    let requested = args.n.unwrap_or(40).clamp(1, 5000);
    let events = read_jsonl_values(&perf_log_file());
    if events.is_empty() {
        print_out(cli, "No perf records found.");
        emit_json(
            cli,
            &json!({
                "ok": true,
                "count": 0,
                "recent": [],
                "summary": {
                    "start": {"count": 0},
                    "stop": {"count": 0}
                }
            }),
        );
        return Ok(0);
    }

    let perf_events = events
        .iter()
        .filter(|e| {
            matches!(
                e.get("action").and_then(|v| v.as_str()),
                Some("start") | Some("stop")
            )
        })
        .cloned()
        .collect::<Vec<_>>();

    let mut recent = perf_events
        .iter()
        .rev()
        .take(requested)
        .cloned()
        .collect::<Vec<_>>();
    recent.reverse();

    if !cli.quiet {
        let start_summary = compute_total_stats(&perf_events, "start");
        let stop_summary = compute_total_stats(&perf_events, "stop");
        let fmt = |v: Option<f64>| -> String {
            v.map(|n| format!("{:.1}", n))
                .unwrap_or_else(|| "-".to_string())
        };
        print_out(
            cli,
            format!(
                "start: count={} avg={}ms p50={}ms p95={}ms",
                start_summary
                    .get("count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                fmt(start_summary.get("avg_ms").and_then(|v| v.as_f64())),
                fmt(start_summary.get("p50_ms").and_then(|v| v.as_f64())),
                fmt(start_summary.get("p95_ms").and_then(|v| v.as_f64())),
            ),
        );
        print_out(
            cli,
            format!(
                "stop:  count={} avg={}ms p50={}ms p95={}ms",
                stop_summary
                    .get("count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                fmt(stop_summary.get("avg_ms").and_then(|v| v.as_f64())),
                fmt(stop_summary.get("p50_ms").and_then(|v| v.as_f64())),
                fmt(stop_summary.get("p95_ms").and_then(|v| v.as_f64())),
            ),
        );
        print_out(cli, "recent:");
        for event in &recent {
            let ts = event.get("ts").and_then(|v| v.as_str()).unwrap_or("-");
            let action = event.get("action").and_then(|v| v.as_str()).unwrap_or("-");
            let total = event
                .get("total_ms")
                .and_then(|v| v.as_f64())
                .map(|n| format!("{:.1}", n))
                .unwrap_or_else(|| "-".to_string());
            let dominant = dominant_phase(event)
                .map(|(name, ms)| format!("{}={:.1}ms", name, ms))
                .unwrap_or_else(|| "-".to_string());
            print_out(
                cli,
                format!(
                    "  {} {} total={}ms dominant={}",
                    ts, action, total, dominant
                ),
            );
        }
    }

    emit_json(
        cli,
        &json!({
            "ok": true,
            "count": perf_events.len(),
            "recent": recent,
            "summary": {
                "start": compute_total_stats(&perf_events, "start"),
                "stop": compute_total_stats(&perf_events, "stop")
            }
        }),
    );

    Ok(0)
}

pub(crate) fn collect_recent_session_dirs(limit: usize) -> Result<Vec<PathBuf>, AppError> {
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

pub(crate) fn collect_recent_session_rows(limit: usize) -> Result<Vec<SessionListRow>, AppError> {
    let session_dirs = collect_recent_session_dirs(limit)?;
    Ok(session_dirs
        .iter()
        .map(|dir| build_list_row(dir))
        .collect::<Vec<_>>())
}

pub(crate) fn resolve_recent_session_dir(rank: usize) -> Result<PathBuf, AppError> {
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

pub(crate) fn resolve_session_dir_by_id(session_id: &str) -> Result<PathBuf, AppError> {
    if session_id.contains('/') || session_id.contains("..") {
        return Err(app_error(8, format!("Invalid session id: {}", session_id)));
    }

    let path = sessions_dir().join(session_id);
    if !path.is_dir() {
        return Err(app_error(
            8,
            format!(
                "Session not found: {} (run 'riff list' to see available ids)",
                session_id
            ),
        ));
    }

    Ok(path)
}

pub(crate) fn cmd_list(cli: &Cli, args: &ListArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let requested = args.n.unwrap_or(10);
    let limit = requested.clamp(1, 200);
    let rows = collect_recent_session_rows(limit)?;
    if rows.is_empty() {
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

pub(crate) fn cmd_copy(_cli: &Cli, args: &CopyArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let requested_rank = args.n.unwrap_or(1);
    let (_, transcript) = load_recent_session_transcript(requested_rank)?;

    // Intentionally raw stdout only, so this can be piped/copied easily.
    println!("{transcript}");
    Ok(0)
}

fn load_recent_session_transcript(rank: usize) -> Result<(PathBuf, String), AppError> {
    let session_dir = resolve_recent_session_dir(rank)?;
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

    let transcript = transcript.trim().to_string();
    if transcript.is_empty() {
        return Err(app_error(
            8,
            format!("No transcript found for session: {}", session_dir.display()),
        ));
    }

    Ok((session_dir, transcript))
}

fn copy_text_to_pbcopy(text: &str) -> Result<(), AppError> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| app_error(1, format!("Failed to run pbcopy: {e}")))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| app_error(1, "Failed to open pbcopy stdin"))?;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| app_error(1, format!("Failed writing transcript to pbcopy: {e}")))?;
    }

    let status = child
        .wait()
        .map_err(|e| app_error(1, format!("Failed waiting for pbcopy: {e}")))?;
    if !status.success() {
        return Err(app_error(
            1,
            format!("pbcopy exited with status {:?}", status.code()),
        ));
    }

    Ok(())
}

fn paste_clipboard_to_focused_app() -> Result<(), AppError> {
    let status = Command::new("osascript")
        .args([
            "-e",
            "tell application \"System Events\" to keystroke \"v\" using command down",
        ])
        .status()
        .map_err(|e| app_error(1, format!("Failed to run osascript for paste: {e}")))?;

    if !status.success() {
        return Err(app_error(
            1,
            format!("osascript paste failed with status {:?}", status.code()),
        ));
    }
    Ok(())
}

pub(crate) fn cmd_send(cli: &Cli, args: &SendArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let requested_rank = args.n.unwrap_or(1);
    let (session_dir, transcript) = load_recent_session_transcript(requested_rank)?;
    let session_id = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    if cli.dry_run {
        print_out(
            cli,
            format!(
                "[dry-run] Would send transcript from session {} ({} chars)",
                session_id,
                transcript.chars().count()
            ),
        );
        emit_json(
            cli,
            &json!({
                "ok": true,
                "action": "send",
                "session_id": session_id,
                "rank": requested_rank,
                "chars": transcript.chars().count(),
                "dry_run": true
            }),
        );
        return Ok(0);
    }

    if !command_exists("pbcopy") {
        return Err(app_error(
            7,
            "pbcopy is unavailable; cannot send transcript",
        ));
    }
    if !command_exists("osascript") {
        return Err(app_error(
            7,
            "osascript is unavailable; cannot send transcript",
        ));
    }

    copy_text_to_pbcopy(&transcript)?;
    // Slight delay improves reliability before issuing Cmd+V to focused app.
    thread::sleep(Duration::from_millis(80));
    paste_clipboard_to_focused_app()?;

    print_out(
        cli,
        format!(
            "Sent transcript from session {} to focused app.",
            session_id
        ),
    );
    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "send",
            "session_id": session_id,
            "rank": requested_rank,
            "chars": transcript.chars().count(),
            "dry_run": false
        }),
    );

    Ok(0)
}

pub(crate) fn cmd_show(_cli: &Cli, args: &ShowArgs) -> Result<i32, AppError> {
    ensure_dirs()?;

    let session_dir = resolve_session_dir_by_id(&args.session_id)?;
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
