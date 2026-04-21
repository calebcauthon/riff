use crate::cli::Cli;
use crate::error::{app_error, AppError};
use crate::models::ShotMeta;
use crate::{append_jsonl, now_iso, print_out, print_verbose, round3, SUPPORTED_IMAGE_EXTS};
use serde_json::json;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

pub(crate) fn detect_screenshot_dir(
    explicit: Option<&Path>,
    cli: &Cli,
) -> Result<PathBuf, AppError> {
    if let Some(p) = explicit {
        let expanded = expand_tilde(p);
        if expanded.is_dir() {
            return Ok(expanded);
        }
        return Err(app_error(
            3,
            format!(
                "Screenshot directory does not exist: {}",
                expanded.display()
            ),
        ));
    }

    let defaults = Command::new("defaults")
        .args(["read", "com.apple.screencapture", "location"])
        .output();

    if let Ok(out) = defaults {
        if out.status.success() {
            let candidate = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !candidate.is_empty() {
                let p = expand_tilde(Path::new(&candidate));
                if p.is_dir() {
                    print_verbose(
                        cli,
                        format!("Detected screenshot dir from defaults: {}", p.display()),
                    );
                    return Ok(p);
                }
            }
        }
    }

    let fallback = home_dir().join("Desktop");
    if fallback.is_dir() {
        print_verbose(
            cli,
            format!("Falling back to screenshot dir: {}", fallback.display()),
        );
        return Ok(fallback);
    }

    Err(app_error(
        3,
        format!(
            "Could not detect screenshot directory from macOS defaults or fallback {}",
            fallback.display()
        ),
    ))
}

fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    path.to_path_buf()
}

fn home_dir() -> PathBuf {
    env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub(crate) fn file_mtime_epoch(path: &Path) -> Option<f64> {
    let md = fs::metadata(path).ok()?;
    let modified = md.modified().ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

fn find_session_screenshots(
    source_dir: &Path,
    started_epoch: f64,
    ended_epoch: f64,
) -> Vec<(PathBuf, f64)> {
    let mut files = Vec::new();

    let entries = match fs::read_dir(source_dir) {
        Ok(e) => e,
        Err(_) => return files,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if !SUPPORTED_IMAGE_EXTS.contains(&ext.as_str()) {
            continue;
        }

        let Some(mtime) = file_mtime_epoch(&path) else {
            continue;
        };

        if (started_epoch - 1.0..=ended_epoch + 2.0).contains(&mtime) {
            files.push((path, mtime));
        }
    }

    files.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    files
}

pub(crate) fn move_session_screenshots(
    source_dir: &Path,
    target_dir: &Path,
    started_epoch: f64,
    ended_epoch: f64,
    events_path: &Path,
    start_index: usize,
    cli: &Cli,
) -> Result<Vec<ShotMeta>, AppError> {
    let mut out = Vec::new();
    let shots = find_session_screenshots(source_dir, started_epoch, ended_epoch);

    for (index, (source, mtime)) in shots.into_iter().enumerate() {
        let shot_id = start_index + index + 1;
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| "png".to_string());
        let dest_name = format!("shot-{shot_id:03}.{ext}");
        let dest_abs = target_dir.join(&dest_name);
        let dest_rel = format!("screenshots/{dest_name}");
        let audio_sec = (mtime - started_epoch).max(0.0);

        if cli.dry_run {
            print_out(
                cli,
                format!(
                    "[dry-run] Would copy {} -> {} and delete source",
                    source.display(),
                    dest_abs.display()
                ),
            );
        } else {
            fs::copy(&source, &dest_abs).map_err(|e| {
                app_error(
                    1,
                    format!(
                        "Failed to copy screenshot {} -> {}: {e}",
                        source.display(),
                        dest_abs.display()
                    ),
                )
            })?;
            fs::remove_file(&source).map_err(|e| {
                app_error(
                    1,
                    format!("Failed to delete screenshot {}: {e}", source.display()),
                )
            })?;

            append_jsonl(
                events_path,
                &json!({
                    "ts": now_iso(),
                    "type": "screenshot_moved",
                    "id": shot_id,
                    "source": source,
                    "dest": dest_rel,
                    "audioSec": round3(audio_sec),
                    "mtime_epoch": round3(mtime),
                }),
            )?;
        }

        out.push(ShotMeta {
            shot_id,
            dest_rel_path: dest_rel,
            audio_sec,
            app_name: None,
            app_bundle_id: None,
            app_pid: None,
            window_title: None,
            app_capture_error: None,
            proc_cpu_percent: None,
            proc_mem_percent: None,
            proc_rss_kb: None,
            proc_elapsed: None,
            proc_state: None,
            proc_command: None,
            proc_capture_error: None,
        });
    }

    Ok(out)
}
