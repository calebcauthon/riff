use crate::error::{app_error, AppError};
use crate::history::{
    collect_recent_session_rows, extract_transcript_from_note, format_duration_compact,
    read_jsonl_values, read_transcript_text_for_session, session_duration_seconds,
    session_started_iso,
};
use crate::models::{ClipboardMeta, SessionListRow, SessionState, ShotMeta};
use crate::paths::sessions_dir;
use crate::shot_modules::{build_shot_output_variants, ShotOutputVariant};
use crate::SUPPORTED_IMAGE_EXTS;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn inject_annotation_markers(
    transcript: &str,
    shots: &[ShotMeta],
    clips: &[ClipboardMeta],
    audio_duration_sec: Option<f64>,
) -> String {
    let mut clean = transcript.trim().to_string();
    for _ in 0..4 {
        let (next, changed) = strip_annotation_markers_once(&clean);
        clean = next;
        if !changed {
            break;
        }
    }
    let clean = clean.trim();
    let mut markers = shots
        .iter()
        .map(|s| (s.audio_sec, format!("[{}]", shot_marker_label(s))))
        .chain(
            clips
                .iter()
                .map(|c| (c.audio_sec, format!("[Clipboard {}]", c.clip_id))),
        )
        .collect::<Vec<_>>();
    markers.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    if clean.is_empty() {
        if markers.is_empty() {
            return "_No transcript available._".to_string();
        }
        let mut lines = vec!["_No transcript available._".to_string(), String::new()];
        for (_, marker) in markers {
            lines.push(marker);
        }
        return lines.join("\n");
    }

    if markers.is_empty() {
        return clean.to_string();
    }

    let Some(duration) = audio_duration_sec else {
        let tail = markers
            .iter()
            .map(|(_, m)| m.clone())
            .collect::<Vec<_>>()
            .join(" ");
        return format!("{}\n\n{}", clean, tail);
    };

    if duration <= 0.0 {
        let tail = markers
            .iter()
            .map(|(_, m)| m.clone())
            .collect::<Vec<_>>()
            .join(" ");
        return format!("{}\n\n{}", clean, tail);
    }

    let mut tokens = clean
        .split_whitespace()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    if tokens.is_empty() {
        return clean.to_string();
    }

    let base_len = tokens.len();
    let mut inserted = 0usize;

    for (audio_sec, marker) in markers {
        let ratio = (audio_sec / duration).clamp(0.0, 1.0);
        let mut idx = ((base_len as f64) * ratio).round() as usize;
        idx = idx.min(tokens.len());
        let insert_at = (idx + inserted).min(tokens.len());
        tokens.insert(insert_at, marker);
        inserted += 1;
    }

    tokens.join(" ")
}

fn is_annotation_marker_body(body: &str) -> bool {
    let t = body.trim();
    if let Some(rest) = t.strip_prefix("Screenshot ") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    if let Some(rest) = t.strip_prefix("Clipboard ") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    if let Some((prefix, suffix)) = t.rsplit_once(" Screenshot ") {
        return !prefix.trim().is_empty()
            && !suffix.is_empty()
            && suffix.chars().all(|c| c.is_ascii_digit());
    }
    false
}

fn strip_annotation_markers_once(input: &str) -> (String, bool) {
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let bytes = input.as_bytes();
    let mut changed = false;

    while i < bytes.len() {
        if bytes[i] != b'[' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        let start = i;
        let mut end = i + 1;
        while end < bytes.len() && bytes[end] != b']' {
            end += 1;
        }
        if end >= bytes.len() {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        let body = &input[start + 1..end];
        if is_annotation_marker_body(body) {
            changed = true;
            i = end + 1;
            continue;
        }

        out.push_str(&input[start..=end]);
        i = end + 1;
    }

    (out, changed)
}

fn format_hms(seconds: f64) -> String {
    let sec = seconds.round().max(0.0) as i64;
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    let s = sec % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn clip_preview(text: &str, max_chars: usize) -> String {
    let single_line = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let normalized = single_line.trim();
    let chars = normalized.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        normalized.to_string()
    } else {
        let mut out = chars[..max_chars].iter().collect::<String>();
        out.push_str("...");
        out
    }
}

fn images_visually_equal(a: &Path, b: &Path) -> bool {
    let Ok(img_a) = image::open(a) else {
        return false;
    };
    let Ok(img_b) = image::open(b) else {
        return false;
    };
    if img_a.width() != img_b.width() || img_a.height() != img_b.height() {
        return false;
    }
    img_a.to_rgba8().as_raw() == img_b.to_rgba8().as_raw()
}

fn shot_marker_label(shot: &ShotMeta) -> String {
    if let Some(app_name) = shot
        .app_name
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        format!("{app_name} Screenshot {}", shot.shot_id)
    } else {
        format!("Screenshot {}", shot.shot_id)
    }
}

fn shot_context_text(shot: &ShotMeta) -> Option<String> {
    let mut parts = Vec::<String>::new();
    if let Some(name) = shot.app_name.as_ref() {
        parts.push(format!("App: {}", name));
    }
    if let Some(bundle) = shot.app_bundle_id.as_ref() {
        parts.push(format!("Bundle: {}", bundle));
    }
    if let Some(pid) = shot.app_pid {
        parts.push(format!("PID: {}", pid));
    }
    if let Some(window_title) = shot.window_title.as_ref() {
        parts.push(format!("Window: {}", window_title));
    }
    if parts.is_empty() {
        if let Some(reason) = shot.app_capture_error.as_ref() {
            parts.push(format!("App metadata unavailable: {}", reason));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn shot_process_summary_text(shot: &ShotMeta) -> Option<String> {
    let mut parts = Vec::<String>::new();
    if let Some(pid) = shot.app_pid {
        parts.push(format!("pid={pid}"));
    }
    if let Some(cpu) = shot.proc_cpu_percent {
        parts.push(format!("cpu={cpu:.1}%"));
    }
    if let Some(mem) = shot.proc_mem_percent {
        parts.push(format!("mem={mem:.1}%"));
    }
    if let Some(rss) = shot.proc_rss_kb {
        parts.push(format!("rss={} KB", rss));
    }
    if let Some(elapsed) = shot.proc_elapsed.as_ref() {
        parts.push(format!("elapsed={elapsed}"));
    }
    if let Some(state) = shot.proc_state.as_ref() {
        parts.push(format!("state={state}"));
    }
    if let Some(command) = shot.proc_command.as_ref() {
        parts.push(format!("command={command}"));
    }
    if parts.is_empty() {
        if let Some(reason) = shot.proc_capture_error.as_ref() {
            parts.push(format!("unavailable ({reason})"));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

pub(crate) fn build_note(
    state: &SessionState,
    ended_iso: &str,
    shots: &[ShotMeta],
    clips: &[ClipboardMeta],
    transcript: &str,
    transcription_meta: &Value,
    audio_duration_sec: Option<f64>,
) -> String {
    let mut lines = Vec::<String>::new();
    lines.push(format!("# Dictation Session {}", state.session_id));
    lines.push(String::new());
    lines.push(format!("- Started (UTC): {}", state.started_at_iso));
    lines.push(format!("- Ended (UTC): {ended_iso}"));
    lines.push("- Audio: `audio.wav`".to_string());
    lines.push(format!(
        "- Screenshots moved from: `{}`",
        state.screenshot_source_dir
    ));
    lines.push(format!("- Screenshots captured: {}", shots.len()));
    lines.push(format!("- Clipboard captures: {}", clips.len()));

    if let Some(duration) = audio_duration_sec {
        lines.push(format!("- Audio duration: {}", format_hms(duration)));
    }

    let t_status = transcription_meta
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    lines.push(format!("- Transcription: `{t_status}`"));

    if let Some(method) = transcription_meta.get("method").and_then(|v| v.as_str()) {
        lines.push(format!("- Transcription method: `{method}`"));
    }

    if !shots.is_empty() {
        lines.push(String::new());
        lines.push("## Screenshot Metadata".to_string());
        lines.push(String::new());
        let session_dir = Path::new(&state.session_dir);
        for shot in shots {
            let abs_path = session_dir.join(&shot.dest_rel_path);
            lines.push(format!("[Screenshot {}]", shot.shot_id));
            lines.push(format!("- Path: {}", abs_path.display()));
            if let Some(ctx) = shot_context_text(shot) {
                lines.push(format!("- {}", ctx));
            }
            if let Some(proc_summary) = shot_process_summary_text(shot) {
                lines.push(format!("- Process: {}", proc_summary));
            }
            lines.push(String::new());
        }
    }

    if !clips.is_empty() {
        for clip in clips {
            lines.push(format!(
                "Clipboard {}: {}",
                clip.clip_id,
                clip_preview(&clip.text, 120)
            ));
        }
        lines.push(String::new());
    }

    lines.push("## Transcript".to_string());
    lines.push(String::new());

    if !shots.is_empty() {
        let session_dir = Path::new(&state.session_dir);
        for shot in shots {
            let abs_path = session_dir.join(&shot.dest_rel_path);
            lines.push(format!(
                "{}: {}",
                shot_marker_label(shot),
                abs_path.display()
            ));
        }
        lines.push(String::new());
    }

    if transcript.trim().is_empty() {
        lines.push("_No transcript available._".to_string());
    } else {
        lines.push(transcript.trim().to_string());
    }
    lines.push(String::new());

    if !shots.is_empty() {
        lines.push("## Screenshot Footnotes".to_string());
        lines.push(String::new());
        for shot in shots {
            let label = shot_marker_label(shot);
            lines.push(format!(
                "[{}]: {} (t={})",
                label,
                shot.dest_rel_path,
                format_hms(shot.audio_sec)
            ));
        }
        lines.push(String::new());
    }

    if !clips.is_empty() {
        lines.push("## Clipboard Footnotes".to_string());
        lines.push(String::new());
        for clip in clips {
            lines.push(format!(
                "[Clipboard {}]: \"{}\" (t={}, chars={})",
                clip.clip_id,
                clip.text.replace('\n', "\\n"),
                format_hms(clip.audio_sec),
                clip.text.chars().count()
            ));
        }
        lines.push(String::new());
    }

    lines.push("## Files".to_string());
    lines.push(String::new());
    lines.push("- `audio.wav`".to_string());
    lines.push("- `events.jsonl`".to_string());
    lines.push("- `ffmpeg.log`".to_string());
    lines.push("- `transcript.txt` (if available)".to_string());
    lines.push("- `note.html`".to_string());
    lines.push("- `screenshots/`".to_string());
    lines.push("- `screenshots/derived/`".to_string());
    lines.push(String::new());

    lines.join("\n")
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn strip_leading_screenshot_path_block(text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let mut idx = 0usize;
    let mut saw_path_line = false;

    while idx < lines.len() {
        let t = lines[idx].trim();
        if is_screenshot_path_line(t) {
            saw_path_line = true;
            idx += 1;
            continue;
        }
        break;
    }

    if !saw_path_line {
        return text.to_string();
    }

    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }

    lines[idx..].join("\n")
}

fn is_screenshot_path_line(line: &str) -> bool {
    let Some((prefix, path_part)) = line.split_once(": ") else {
        return false;
    };
    path_part.starts_with('/') && is_annotation_marker_body(prefix)
}

pub(crate) fn build_html_note(
    session_id: &str,
    started_iso: &str,
    ended_iso: &str,
    audio_duration_sec: Option<f64>,
    transcription_meta: &Value,
    transcript: &str,
    markdown_for_copy: &str,
    shots: &[ShotMeta],
    clips: &[ClipboardMeta],
    session_dir: &Path,
    sessions_index_href: &str,
) -> String {
    let t_status = transcription_meta
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let t_method = transcription_meta
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("-");

    let mut gallery = String::new();
    for shot in shots {
        let abs = session_dir.join(&shot.dest_rel_path);
        let abs_str = abs.display().to_string();
        let rel_url = shot.dest_rel_path.clone();
        let ctx_text = shot_context_text(shot);
        let context_html = ctx_text
            .as_ref()
            .map(|s| format!(r#"<div class="ctx">App: {}</div>"#, html_escape(s)))
            .unwrap_or_default();
        let process_html = shot_process_summary_text(shot)
            .map(|s| format!(r#"<div class="sys">System: {}</div>"#, html_escape(&s)))
            .unwrap_or_default();
        let mut variants_html = String::new();
        let mut variants = build_shot_output_variants(session_dir, shot);
        if variants.is_empty() {
            variants.push(ShotOutputVariant {
                module_id: "source",
                module_name: "Source",
                rel_url: rel_url.clone(),
                abs_path: abs_str.clone(),
            });
        }
        let transcript_bytes = fs::read(&abs).ok();
        let selected_module = variants.iter().find_map(|variant| {
            let variant_path = Path::new(&variant.abs_path);
            let byte_match = transcript_bytes
                .as_ref()
                .and_then(|bytes| {
                    fs::read(variant_path)
                        .ok()
                        .filter(|candidate| candidate == bytes)
                })
                .is_some();
            if byte_match || images_visually_equal(&abs, variant_path) {
                Some(variant.module_id)
            } else {
                None
            }
        });
        for variant in variants {
            let is_selected = selected_module == Some(variant.module_id);
            let selected_class = if is_selected { " selected" } else { "" };
            let selected_badge = if is_selected {
                r#"<span class="active-pill">In transcript</span>"#
            } else {
                ""
            };
            let use_button = if is_selected {
                r#"<button class="ispy-btn tiny use-variant" disabled aria-disabled="true">In transcript</button>"#
                    .to_string()
            } else {
                format!(
                    r#"<button class="ispy-btn tiny use-variant" data-session-id="{session_id}" data-shot-id="{shot_id}" data-module="{module_id}">Use for transcript</button>"#,
                    session_id = html_escape(session_id),
                    shot_id = shot.shot_id,
                    module_id = variant.module_id,
                )
            };
            variants_html.push_str(&format!(
                r#"<figure class="variant{selected_class}" data-module="{module_id}" data-shot-id="{shot_id}"><div class="variant-head"><div class="variant-title"><figcaption>{module_name}</figcaption>{selected_badge}</div><div class="variant-actions">{use_button}<button class="ispy-btn tiny annotate-image" data-url="{url}" data-path="{path}">Annotate</button><button class="ispy-btn tiny copy-image" data-url="{url}" data-path="{path}">Copy image</button></div></div><div class="img-wrap"><a href="{url}" target="_blank" rel="noreferrer"><img src="{url}" alt="Screenshot {shot_id} - {module_name}" loading="lazy" /></a></div></figure>"#,
                selected_class = selected_class,
                selected_badge = selected_badge,
                use_button = use_button,
                module_id = variant.module_id,
                module_name = html_escape(variant.module_name),
                url = html_escape(&variant.rel_url),
                path = html_escape(&variant.abs_path),
                shot_id = shot.shot_id,
            ));
        }
        gallery.push_str(&format!(
            r#"<article class="shot-card"><div class="shot-head"><h3>Screenshot {}</h3><div class="path">{}</div>{}{}</div><div class="variants">{}</div></article>"#,
            shot.shot_id,
            html_escape(&abs_str),
            context_html,
            process_html,
            variants_html
        ));
    }

    let mut clip_cards = String::new();
    for clip in clips {
        let preview = clip_preview(&clip.text, 300);
        clip_cards.push_str(&format!(
            r#"<div class="card"><div class="card-head"><strong>Clipboard {}</strong><button class="ispy-btn small copy-clip" data-text="{}">Copy text</button></div><div class="transcript">{}</div><div class="path">t={} · {} chars</div></div>"#,
            clip.clip_id,
            html_escape(&clip.text),
            html_escape(&preview),
            html_escape(&format_hms(clip.audio_sec)),
            clip.text.chars().count()
        ));
    }

    let duration = format_duration_compact(audio_duration_sec);
    let transcript_core = strip_leading_screenshot_path_block(transcript.trim());
    let mut transcript_text = if transcript_core.trim().is_empty() {
        "_No transcript available._".to_string()
    } else {
        transcript_core.trim().to_string()
    };
    if !shots.is_empty() {
        let mut path_lines = String::new();
        for shot in shots {
            let abs = session_dir.join(&shot.dest_rel_path);
            path_lines.push_str(&format!("{}: {}\n", shot_marker_label(shot), abs.display()));
        }
        transcript_text = format!("{}\n\n{}", path_lines.trim_end(), transcript_text);
    }

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Dictation {session_id}</title>
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@excalidraw/excalidraw@0.18.0/dist/prod/index.min.css" />
  <style>
    body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; margin: 0; background: #f5f7fb; color: #111827; }}
    .wrap {{ max-width: 1000px; margin: 0 auto; padding: 24px; }}
    h1 {{ margin: 0 0 12px; font-size: 28px; }}
    h2 {{ margin-top: 0; }}
    .meta {{ background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 14px 16px; margin-bottom: 16px; }}
    .meta ul {{ margin: 0; padding-left: 18px; line-height: 1.6; }}
    .panel {{ background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 16px; margin-bottom: 16px; }}
    .actions {{ display: flex; align-items: center; gap: 8px; margin-bottom: 12px; }}
    .transcript-head {{ display: flex; align-items: center; gap: 4px; margin-bottom: 12px; }}
    .nav {{ margin: 0 0 14px; }}
    .nav a {{ color: #1d4ed8; text-decoration: none; font-weight: 600; }}
    .nav a:hover {{ text-decoration: underline; }}
    .status {{ font-size: 12px; color: #475569; }}
    .ispy-btn {{ background: #111827; color: #fff; border: 0; border-radius: 8px; padding: 8px 12px; font-size: 13px; cursor: pointer; }}
    .ispy-btn:hover {{ background: #1f2937; }}
    .ispy-btn.small {{ padding: 6px 10px; font-size: 12px; }}
    .ispy-btn.tiny {{ padding: 3px 8px; font-size: 11px; border-radius: 6px; }}
    .transcript {{ white-space: pre-wrap; line-height: 1.6; font-size: 15px; }}
    .grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(280px, 1fr)); gap: 12px; }}
    .shot-grid {{ display: grid; grid-template-columns: 1fr; gap: 12px; }}
    .card {{ background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 10px; margin: 0; }}
    .card-head {{ display: flex; justify-content: space-between; align-items: center; gap: 8px; margin-bottom: 8px; }}
    .card img {{ width: 100%; height: auto; border-radius: 8px; display: block; }}
    .card figcaption {{ font-weight: 600; margin: 0; }}
    .shot-card {{ border: 1px solid #e5e7eb; border-radius: 12px; background: #fff; padding: 12px; }}
    .shot-head h3 {{ margin: 0 0 6px; font-size: 16px; }}
    .variants {{ margin-top: 10px; display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 10px; }}
    .variant {{ border: 1px solid #e5e7eb; border-radius: 10px; padding: 8px; background: #fff; margin: 0; }}
    .variant.selected {{ border-color: #0f766e; box-shadow: 0 0 0 2px rgba(15, 118, 110, 0.14); }}
    .variant-head {{ display: flex; justify-content: space-between; align-items: center; margin-bottom: 6px; gap: 8px; }}
    .variant-title {{ display: flex; align-items: center; gap: 6px; min-width: 0; }}
    .variant-actions {{ display: flex; align-items: center; gap: 6px; }}
    .variant-head figcaption {{ font-size: 12px; font-weight: 700; color: #374151; }}
    .active-pill {{ font-size: 10px; font-weight: 700; letter-spacing: 0.02em; color: #0f766e; background: #ccfbf1; border: 1px solid #99f6e4; padding: 2px 6px; border-radius: 999px; white-space: nowrap; }}
    .img-wrap {{ position: relative; border-radius: 8px; overflow: hidden; background: #f8fafc; }}
    .img-wrap a {{ display: block; }}
    .img-wrap img {{ width: 100%; height: auto; display: block; }}
    .path {{ color: #6b7280; margin-top: 8px; font-size: 12px; word-break: break-all; }}
    .ctx {{ color: #374151; margin-top: 6px; font-size: 12px; line-height: 1.5; }}
    .sys {{ color: #111827; margin-top: 6px; font-size: 12px; line-height: 1.5; }}
    .annotator-modal {{ position: fixed; inset: 0; background: rgba(15, 23, 42, 0.72); display: none; align-items: center; justify-content: center; padding: 16px; z-index: 9999; }}
    .annotator-modal.open {{ display: flex; }}
    .annotator-panel {{ width: min(1200px, 100%); max-height: calc(100vh - 32px); overflow: auto; background: #fff; border-radius: 12px; border: 1px solid #e5e7eb; padding: 12px; }}
    .annotator-toolbar {{ display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-bottom: 10px; }}
    .annotator-toolbar-spacer {{ flex: 1; }}
    .annotator-close-btn {{ min-width: 32px; padding: 4px 10px; font-size: 16px; line-height: 1; }}
    .annotator-stage {{ position: relative; border: 1px solid #e5e7eb; border-radius: 8px; overflow: hidden; background: #f8fafc; min-height: 420px; }}
    .annotator-loading {{ position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; color: #475569; font-size: 14px; z-index: 2; pointer-events: none; }}
    .annotator-loading.hidden {{ display: none; }}
    .annotator-host {{ width: 100%; height: min(76vh, 900px); }}
    .annotator-help {{ margin-top: 8px; font-size: 12px; color: #64748b; }}
  </style>
</head>
<body>
  <div class="wrap">
    <h1>Dictation Session {session_id}</h1>
    <p class="nav"><a href="{sessions_index_href}">Browse all sessions</a></p>

    <section class="meta">
      <div class="actions">
        <button id="copyMarkdownBtn" class="ispy-btn">Copy markdown</button>
        <span id="copyStatus" class="status"></span>
      </div>
      <ul>
        <li><strong>Started (UTC):</strong> {started_iso}</li>
        <li><strong>Ended (UTC):</strong> {ended_iso}</li>
        <li><strong>Audio duration:</strong> {duration}</li>
        <li><strong>Screenshots:</strong> {screenshots}</li>
        <li><strong>Clipboard captures:</strong> {clips}</li>
        <li><strong>Transcription status:</strong> {t_status}</li>
        <li><strong>Transcription method:</strong> {t_method}</li>
      </ul>
    </section>

    <section class="panel">
      <div class="transcript-head">
        <h2 style="margin: 0;">Transcript</h2>
        <button id="copyTranscriptBtn" class="ispy-btn tiny">Copy</button>
      </div>
      <div class="transcript">{transcript_html}</div>
    </section>

    <section class="panel">
      <h2>Screenshots</h2>
      <div class="path">Each input screenshot is rendered through all active output modules.</div>
      {gallery_html}
    </section>

    <section class="panel">
      <h2>Clipboard</h2>
      {clipboard_html}
    </section>
  </div>

  <div id="annotatorModal" class="annotator-modal" aria-hidden="true">
    <div class="annotator-panel">
      <div class="annotator-toolbar">
        <button id="annotatorSaveCloseBtn" class="ispy-btn small">Save and close</button>
        <button id="annotatorDownloadBtn" class="ispy-btn small">Download annotated PNG</button>
        <span class="annotator-toolbar-spacer"></span>
        <span id="annotatorStatus" class="status"></span>
        <button id="annotatorCloseBtn" class="ispy-btn small annotator-close-btn" aria-label="Close annotator" title="Close annotator">×</button>
      </div>
      <div class="annotator-stage">
        <div id="annotatorLoading" class="annotator-loading">Loading Excalidraw…</div>
        <div id="annotatorHost" class="annotator-host"></div>
      </div>
      <div class="annotator-help">Excalidraw tip: the screenshot is preloaded as an image element you can draw on top of.</div>
    </div>
  </div>

  <textarea id="markdownContent" style="display:none;">{markdown_html}</textarea>
  <textarea id="transcriptContent" style="display:none;">{transcript_copy_html}</textarea>
  <script>
    (function () {{
      function openFallbackAnnotator(btn) {{
        if (!btn) return;
        var url = btn.getAttribute('data-url') || '';
        var path = btn.getAttribute('data-path') || url;
        var modal = document.getElementById('annotatorModal');
        var loading = document.getElementById('annotatorLoading');
        if (!modal) return;
        modal.classList.add('open');
        modal.setAttribute('aria-hidden', 'false');
        if (loading) {{
          loading.classList.remove('hidden');
          loading.textContent = 'Loading Excalidraw…';
        }}
        if (typeof window.__ispyOpenExcalidraw === 'function') {{
          window.__ispyOpenExcalidraw(url, path);
        }} else if (loading) {{
          loading.textContent = 'Annotator runtime not ready yet. Please wait a second and click Annotate again.';
        }}
      }}

      document.addEventListener('click', function (evt) {{
        var target = evt.target;
        if (!target) return;
        var btn = target.closest ? target.closest('.annotate-image') : null;
        if (!btn) return;
        evt.preventDefault();
        openFallbackAnnotator(btn);
      }});
    }})();
  </script>
  <script>
    const copyStatus = document.getElementById('copyStatus');
    function setStatus(msg) {{
      if (!copyStatus) return;
      copyStatus.textContent = msg;
      window.setTimeout(() => {{
        if (copyStatus.textContent === msg) copyStatus.textContent = '';
      }}, 2000);
    }}

    async function copyText(text, successMessage) {{
      if (!navigator.clipboard || !navigator.clipboard.writeText) {{
        throw new Error('Clipboard text API unavailable');
      }}
      await navigator.clipboard.writeText(text);
      setStatus(successMessage);
    }}

    function flashButtonLabel(btn, tempLabel, timeoutMs) {{
      if (!btn) return;
      const original = btn.dataset.originalLabel || btn.textContent || '';
      btn.dataset.originalLabel = original;
      btn.textContent = tempLabel;
      window.setTimeout(() => {{
        btn.textContent = original;
      }}, timeoutMs);
    }}

    document.getElementById('copyMarkdownBtn')?.addEventListener('click', async () => {{
      const markdown = document.getElementById('markdownContent')?.value || '';
      try {{
        await copyText(markdown, 'Markdown copied');
      }} catch (err) {{
        setStatus('Could not copy markdown');
      }}
    }});

    const copyTranscriptBtn = document.getElementById('copyTranscriptBtn');
    copyTranscriptBtn?.addEventListener('click', async () => {{
      const transcript = document.getElementById('transcriptContent')?.value || '';
      try {{
        await copyText(transcript, 'Transcript copied');
        flashButtonLabel(copyTranscriptBtn, 'Copied', 1000);
      }} catch (err) {{
        setStatus('Could not copy transcript');
      }}
    }});

    document.querySelectorAll('.copy-image').forEach((btn) => {{
      btn.addEventListener('click', async () => {{
        const url = btn.dataset.url || '';
        const path = btn.dataset.path || url;

        try {{
          if (!navigator.clipboard || !window.ClipboardItem || !navigator.clipboard.write) {{
            throw new Error('Image clipboard API unavailable');
          }}

          const response = await fetch(url);
          if (!response.ok) throw new Error('Failed to fetch image');
          const blob = await response.blob();
          const type = blob.type || 'image/png';
          await navigator.clipboard.write([new ClipboardItem({{ [type]: blob }})]);
          setStatus('Image copied');
        }} catch (err) {{
          try {{
            await copyText(path, 'Copied image path');
          }} catch (_err) {{
            setStatus('Could not copy image');
          }}
        }}
      }});
    }});

    document.querySelectorAll('.copy-clip').forEach((btn) => {{
      btn.addEventListener('click', async () => {{
        const text = btn.dataset.text || '';
        try {{
          await copyText(text, 'Clipboard text copied');
        }} catch (err) {{
          setStatus('Could not copy clipboard text');
        }}
      }});
    }});

    const annotatorModal = document.getElementById('annotatorModal');
    const annotatorStatus = document.getElementById('annotatorStatus');

    function setAnnotatorStatus(message) {{
      if (!annotatorStatus) return;
      annotatorStatus.textContent = message;
      window.setTimeout(() => {{
        if (annotatorStatus.textContent === message) annotatorStatus.textContent = '';
      }}, 2200);
    }}

    function closeAnnotator() {{
      if (!annotatorModal) return;
      annotatorModal.classList.remove('open');
      annotatorModal.setAttribute('aria-hidden', 'true');
    }}

    document.getElementById('annotatorCloseBtn')?.addEventListener('click', closeAnnotator);
    annotatorModal?.addEventListener('click', (evt) => {{
      if (evt.target === annotatorModal) closeAnnotator();
    }});
    window.addEventListener('keydown', (evt) => {{
      if (evt.key === 'Escape' && annotatorModal?.classList.contains('open')) closeAnnotator();
    }});

    document.getElementById('annotatorSaveCloseBtn')?.addEventListener('click', async () => {{
      if (typeof window.__ispySaveExcalidraw !== 'function') {{
        setAnnotatorStatus('Excalidraw not ready');
        return;
      }}
      try {{
        await window.__ispySaveExcalidraw();
        setAnnotatorStatus('Saved');
        closeAnnotator();
      }} catch (_err) {{
        setAnnotatorStatus('Could not save scene');
      }}
    }});

    document.getElementById('annotatorDownloadBtn')?.addEventListener('click', async () => {{
      if (typeof window.__ispyDownloadExcalidrawPng !== 'function') {{
        setAnnotatorStatus('Excalidraw not ready');
        return;
      }}
      try {{
        await window.__ispyDownloadExcalidrawPng();
        setAnnotatorStatus('Annotated PNG downloaded');
      }} catch (_err) {{
        setAnnotatorStatus('Could not export PNG');
      }}
    }});

    document.querySelectorAll('.use-variant').forEach((btn) => {{
      btn.addEventListener('click', async () => {{
        const sessionId = btn.dataset.sessionId || '';
        const module = btn.dataset.module || '';
        const shotId = Number(btn.dataset.shotId || '0');
        if (!sessionId || !module || !shotId) {{
          setStatus('Missing variant metadata');
          return;
        }}

        const original = btn.textContent || 'Use for transcript';
        btn.disabled = true;
        btn.textContent = 'Applying...';
        try {{
          const candidates = [];
          if (window.location.protocol !== 'file:') {{
            candidates.push('/use-screenshot');
          }}
          candidates.push('http://127.0.0.1:8766/use-screenshot');

          let applied = false;
          let lastError = null;
          for (const endpoint of candidates) {{
            try {{
              const res = await fetch(endpoint, {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{
                  session_id: sessionId,
                  shot_id: shotId,
                  module
                }})
              }});
              const payload = await res.json().catch(() => ({{}}));
              if (!res.ok || !payload.ok) {{
                throw new Error(payload.error || 'Failed to apply variant');
              }}
              applied = true;
              break;
            }} catch (endpointErr) {{
              lastError = endpointErr;
            }}
          }}
          if (!applied) {{
            throw lastError || new Error('Failed to apply variant');
          }}
          setStatus(`Now using ${{module}} for Screenshot ${{shotId}}`);
          window.setTimeout(() => window.location.reload(), 450);
        }} catch (err) {{
          setStatus(`Could not apply variant: ${{err.message || 'unknown error'}}`);
          btn.disabled = false;
          btn.textContent = original;
          return;
        }}
      }});
    }});
  </script>
  <script type="importmap">
    {{
      "imports": {{
        "react": "https://esm.sh/react@19.0.0",
        "react/jsx-runtime": "https://esm.sh/react@19.0.0/jsx-runtime",
        "react-dom": "https://esm.sh/react-dom@19.0.0",
        "react-dom/client": "https://esm.sh/react-dom@19.0.0/client"
      }}
    }}
  </script>
  <script type="module">
    window.EXCALIDRAW_ASSET_PATH = 'https://esm.sh/@excalidraw/excalidraw@0.18.0/dist/dev/';
    const EXCALIDRAW_CSS_ID = 'ispy-excalidraw-css';
    const EXCALIDRAW_CSS_URLS = [
      'https://cdn.jsdelivr.net/npm/@excalidraw/excalidraw@0.18.0/dist/prod/index.min.css',
      'https://unpkg.com/@excalidraw/excalidraw@0.18.0/dist/prod/index.min.css',
    ];
    const host = document.getElementById('annotatorHost');
    const annotatorLoading = document.getElementById('annotatorLoading');
    const sceneStoragePrefix = 'ispy-excalidraw-scene-';
    let reactRoot = null;
    let excalidrawAPI = null;
    let excalidrawPkg = null;
    let currentContext = {{ url: '', path: '' }};
    let loaderPromise = null;

    function setLoading(loading, message) {{
      if (!annotatorLoading) return;
      if (message) annotatorLoading.textContent = message;
      if (loading) {{
        annotatorLoading.classList.remove('hidden');
      }} else {{
        annotatorLoading.classList.add('hidden');
      }}
    }}

    function sceneKey(path) {{
      return `${{sceneStoragePrefix}}${{path}}`;
    }}

    function sanitizeAppState(input) {{
      const src = (input && typeof input === 'object') ? input : {{}};
      const allowed = {{
        viewBackgroundColor: src.viewBackgroundColor || '#ffffff',
        theme: src.theme || 'light',
        currentItemStrokeColor: src.currentItemStrokeColor || '#ef4444',
        currentItemBackgroundColor: src.currentItemBackgroundColor || 'transparent',
        currentItemStrokeWidth: Number(src.currentItemStrokeWidth || 2),
        currentItemRoughness: Number(src.currentItemRoughness || 0),
        currentItemOpacity: Number(src.currentItemOpacity || 100),
        currentItemFontFamily: Number(src.currentItemFontFamily || 1),
        currentItemFontSize: Number(src.currentItemFontSize || 20),
      }};
      if (src.zoom && typeof src.zoom === 'object' && typeof src.zoom.value === 'number') {{
        allowed.zoom = {{ value: src.zoom.value }};
      }}
      if (typeof src.scrollX === 'number') allowed.scrollX = src.scrollX;
      if (typeof src.scrollY === 'number') allowed.scrollY = src.scrollY;
      return allowed;
    }}

    function refreshScreenshotPreview(targetPath, targetUrl) {{
      const stamp = Date.now();
      document.querySelectorAll('.annotate-image').forEach((btn) => {{
        if ((btn.dataset.path || '') !== targetPath) return;
        const baseUrl = btn.dataset.url || targetUrl;
        const cacheBusted = `${{baseUrl}}?v=${{stamp}}`;
        btn.dataset.url = baseUrl;
        const variant = btn.closest('.variant');
        if (!variant) return;
        variant.querySelectorAll('button[data-path]').forEach((variantBtn) => {{
          variantBtn.dataset.url = baseUrl;
        }});
        const img = variant.querySelector('img');
        const link = variant.querySelector('a');
        if (img) img.src = cacheBusted;
        if (link) link.href = cacheBusted;
      }});
    }}

    function saveImageEndpoints() {{
      const endpoints = [];
      if (window.location.protocol === 'http:' || window.location.protocol === 'https:') {{
        endpoints.push(`${{window.location.origin}}/save-image`);
      }}
      endpoints.push('http://127.0.0.1:8766/save-image');
      endpoints.push('http://localhost:8766/save-image');
      return [...new Set(endpoints)];
    }}

    function randomId(prefix) {{
      return `${{prefix}}-${{Math.random().toString(36).slice(2, 10)}}`;
    }}

    async function loadModule() {{
      if (loaderPromise) return loaderPromise;
      loaderPromise = (async () => {{
        const ReactModule = await import('https://esm.sh/react@19.0.0');
        const ReactDOMClient = await import('https://esm.sh/react-dom@19.0.0/client');
        const ExcalidrawModule = await import('https://esm.sh/@excalidraw/excalidraw@0.18.0/dist/dev/index.js?external=react,react-dom');
        return {{
          React: ReactModule.default,
          createRoot: ReactDOMClient.createRoot,
          pkg: ExcalidrawModule,
        }};
      }})();
      return loaderPromise;
    }}

    function ensureExcalidrawCss() {{
      if (document.getElementById(EXCALIDRAW_CSS_ID)) return;
      const link = document.createElement('link');
      link.id = EXCALIDRAW_CSS_ID;
      link.rel = 'stylesheet';
      link.href = EXCALIDRAW_CSS_URLS[0];
      link.onerror = () => {{
        if (link.href !== EXCALIDRAW_CSS_URLS[1]) {{
          link.href = EXCALIDRAW_CSS_URLS[1];
        }}
      }};
      document.head.appendChild(link);
    }}

    async function blobToDataUrl(blob) {{
      return await new Promise((resolve, reject) => {{
        const reader = new FileReader();
        reader.onload = () => resolve(String(reader.result || ''));
        reader.onerror = reject;
        reader.readAsDataURL(blob);
      }});
    }}

    async function preloadImageData(url) {{
      const response = await fetch(url);
      if (!response.ok) throw new Error('Failed to fetch screenshot');
      const blob = await response.blob();
      const dataUrl = await blobToDataUrl(blob);
      const dimensions = await new Promise((resolve, reject) => {{
        const img = new Image();
        img.onload = () => resolve({{ width: img.naturalWidth || 1280, height: img.naturalHeight || 720 }});
        img.onerror = reject;
        img.src = dataUrl;
      }});
      return {{
        dataUrl,
        mimeType: blob.type || 'image/png',
        width: dimensions.width,
        height: dimensions.height,
      }};
    }}

    function createImageElement(fileId, width, height) {{
      const now = Date.now();
      return {{
        id: randomId('img'),
        type: 'image',
        x: 0,
        y: 0,
        width,
        height,
        angle: 0,
        strokeColor: 'transparent',
        backgroundColor: 'transparent',
        fillStyle: 'solid',
        strokeWidth: 1,
        strokeStyle: 'solid',
        roughness: 0,
        opacity: 100,
        groupIds: [],
        frameId: null,
        roundness: null,
        seed: Math.floor(Math.random() * 2_000_000_000),
        version: 1,
        versionNonce: Math.floor(Math.random() * 2_000_000_000),
        isDeleted: false,
        boundElements: null,
        updated: now,
        link: null,
        locked: true,
        fileId,
        scale: [1, 1],
        crop: null,
        status: 'saved',
      }};
    }}

    async function buildInitialData(url, path) {{
      const savedRaw = localStorage.getItem(sceneKey(path));
      if (savedRaw) {{
        try {{
          const saved = JSON.parse(savedRaw);
          const savedAppState = sanitizeAppState(saved?.appState);
          const savedPayload = {{
            elements: Array.isArray(saved?.elements) ? saved.elements : [],
            appState: savedAppState,
            files: (saved && typeof saved.files === 'object' && saved.files) ? saved.files : {{}},
          }};
          localStorage.setItem(sceneKey(path), JSON.stringify(savedPayload));
          return {{
            elements: savedPayload.elements,
            appState: savedAppState,
            files: savedPayload.files,
            scrollToContent: true,
          }};
        }} catch (_err) {{}}
      }}

      const image = await preloadImageData(url);
      const fileId = randomId('file');
      const files = {{
        [fileId]: {{
          id: fileId,
          mimeType: image.mimeType,
          dataURL: image.dataUrl,
          created: Date.now(),
          lastRetrieved: Date.now(),
        }},
      }};
      const elements = [createImageElement(fileId, image.width, image.height)];
      return {{
        elements,
        files,
        appState: sanitizeAppState({{}}),
        scrollToContent: true,
      }};
    }}

    async function renderAnnotator(url, path) {{
      if (!host) return;
      ensureExcalidrawCss();
      const resolvedUrl = new URL(url, window.location.href).toString();
      currentContext = {{ url, path, resolvedUrl }};
      setLoading(true, 'Loading Excalidraw…');

      const loaded = await loadModule();
      const React = loaded.React;
      const h = React.createElement;
      const pkg = loaded.pkg;
      excalidrawPkg = pkg;
      if (!reactRoot) {{
        reactRoot = loaded.createRoot(host);
      }}

      const initialData = await buildInitialData(url, path);
      reactRoot.render(
        h(
          'div',
          {{ style: {{ height: '100%' }} }},
          h(pkg.Excalidraw, {{
            key: path,
            initialData,
            excalidrawAPI: (api) => {{
              excalidrawAPI = api;
              setLoading(false);
            }},
            UIOptions: {{
              canvasActions: {{
                saveToActiveFile: false,
              }},
            }},
          }})
        )
      );
    }}

    window.__ispyOpenExcalidraw = async (url, path) => {{
      try {{
        await renderAnnotator(url, path);
      }} catch (err) {{
        setLoading(true, 'Could not load Excalidraw CDN bundle');
        console.error(err);
      }}
    }};

    window.__ispySaveExcalidraw = async () => {{
      if (!excalidrawAPI || !currentContext.path) throw new Error('Excalidraw not ready');
      const persistedAppState = sanitizeAppState(excalidrawAPI.getAppState());
      const payload = {{
        elements: excalidrawAPI.getSceneElementsIncludingDeleted(),
        appState: persistedAppState,
        files: excalidrawAPI.getFiles(),
        savedAt: new Date().toISOString(),
      }};
      localStorage.setItem(sceneKey(currentContext.path), JSON.stringify(payload));

      const blob = await excalidrawPkg.exportToBlob({{
        elements: excalidrawAPI.getSceneElements(),
        appState: {{
          ...excalidrawAPI.getAppState(),
          exportBackground: true,
          exportWithDarkMode: false,
        }},
        files: excalidrawAPI.getFiles(),
        mimeType: 'image/png',
      }});
      const dataUrl = await blobToDataUrl(blob);
      let lastError = '';
      let saved = false;
      for (const endpoint of saveImageEndpoints()) {{
        try {{
          const response = await fetch(endpoint, {{
            method: 'POST',
            headers: {{
              'Content-Type': 'application/json',
            }},
            body: JSON.stringify({{
              url: currentContext.resolvedUrl || currentContext.url,
              absPath: currentContext.path,
              dataUrl,
            }}),
          }});
          if (!response.ok) {{
            const body = await response.text();
            lastError = `${{endpoint}} -> ${{response.status}} ${{body}}`;
            continue;
          }}
          saved = true;
          break;
        }} catch (err) {{
          lastError = `${{endpoint}} -> ${{err}}`;
        }}
      }}
      if (!saved) {{
        throw new Error(`save-image failed across endpoints: ${{lastError || 'unknown'}}`);
      }}
      refreshScreenshotPreview(currentContext.path, currentContext.url);
    }};

    window.__ispyDownloadExcalidrawPng = async () => {{
      if (!excalidrawAPI || !excalidrawPkg) throw new Error('Excalidraw not ready');
      const blob = await excalidrawPkg.exportToBlob({{
        elements: excalidrawAPI.getSceneElements(),
        appState: {{
          ...excalidrawAPI.getAppState(),
          exportBackground: true,
          exportWithDarkMode: false,
        }},
        files: excalidrawAPI.getFiles(),
        mimeType: 'image/png',
      }});
      const downloadName = (currentContext.path.split('/').pop() || 'annotated').replace(/[^a-zA-Z0-9._-]/g, '_');
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `annotated-${{downloadName.replace(/\\.[a-zA-Z0-9]+$/, '')}}.png`;
      a.click();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    }};
  </script>
</body>
</html>
"#,
        session_id = html_escape(session_id),
        started_iso = html_escape(started_iso),
        ended_iso = html_escape(ended_iso),
        duration = html_escape(&duration),
        screenshots = shots.len(),
        clips = clips.len(),
        t_status = html_escape(t_status),
        t_method = html_escape(t_method),
        transcript_html = html_escape(&transcript_text),
        transcript_copy_html = html_escape(&transcript_text),
        markdown_html = html_escape(markdown_for_copy),
        sessions_index_href = html_escape(sessions_index_href),
        gallery_html = if gallery.is_empty() {
            "<div>No screenshots in this session.</div>".to_string()
        } else {
            format!("<div class=\"shot-grid\">{}</div>", gallery)
        },
        clipboard_html = if clip_cards.is_empty() {
            "<div>No clipboard captures in this session.</div>".to_string()
        } else {
            format!("<div class=\"grid\">{}</div>", clip_cards)
        },
    )
}

fn build_sessions_index_html(rows: &[SessionListRow]) -> String {
    let mut entries_html = String::new();
    for row in rows {
        let session_dir = sessions_dir().join(&row.session_id);
        let transcript = read_transcript_text_for_session(&session_dir);
        let transcript = if transcript.trim().is_empty() {
            "No transcript available.".to_string()
        } else {
            transcript.split_whitespace().collect::<Vec<_>>().join(" ")
        };

        let mut thumb_items = String::new();
        let screenshots_dir = session_dir.join("screenshots");
        if let Ok(entries) = fs::read_dir(screenshots_dir) {
            let mut files = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|s| s.to_ascii_lowercase())
                        .map(|ext| SUPPORTED_IMAGE_EXTS.contains(&ext.as_str()))
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>();
            files.sort();

            for path in files.iter().take(8) {
                let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let rel = format!("./{}/screenshots/{}", row.session_id, file_name);
                thumb_items.push_str(&format!(
                    r#"<a class="thumb" href="{rel}" target="_blank" rel="noreferrer"><img src="{rel}" alt="Screenshot thumbnail for {session_id}" loading="lazy" /></a>"#,
                    rel = html_escape(&rel),
                    session_id = html_escape(&row.session_id),
                ));
            }
        }

        let note_href = format!("./{}/note.html", row.session_id);
        entries_html.push_str(&format!(
            r#"<article class="row"><div class="main"><div class="row-top"><a class="session" href="{note_href}">{session_id}</a><span class="meta">{timestamp}</span><span class="meta">{images} images</span><span class="meta">{duration}</span><button class="btn tiny copy-row-transcript" data-href="{note_href}" data-transcript="{transcript_attr}">Copy transcript</button></div><div class="transcript" title="{transcript_title}">{transcript}</div></div><div class="thumbs">{thumbs}</div></article>"#,
            note_href = html_escape(&note_href),
            session_id = html_escape(&row.session_id),
            timestamp = html_escape(&row.timestamp),
            images = row.images,
            duration = html_escape(&row.duration),
            transcript = html_escape(&transcript),
            transcript_attr = html_escape(&transcript),
            transcript_title = html_escape(&transcript),
            thumbs = if thumb_items.is_empty() {
                "<div class=\"thumb-empty\">No screenshots.</div>".to_string()
            } else {
                thumb_items
            },
        ));
    }

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Dictation Sessions</title>
  <style>
    body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; margin: 0; background: #f5f7fb; color: #111827; }}
    .wrap {{ max-width: 1100px; margin: 0 auto; padding: 24px; }}
    h1 {{ margin: 0 0 8px; font-size: 30px; }}
    .sub {{ color: #4b5563; margin: 0 0 18px; }}
    .rows {{ display: flex; flex-direction: column; gap: 12px; }}
    .row {{ background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 10px 12px; display: flex; align-items: center; gap: 12px; }}
    .main {{ min-width: 0; flex: 1; }}
    .row-top {{ display: flex; flex-wrap: nowrap; align-items: center; gap: 10px; margin-bottom: 4px; overflow: hidden; }}
    .session {{ color: #1d4ed8; text-decoration: none; font-weight: 700; }}
    .session:hover {{ text-decoration: underline; }}
    .btn {{ background: #111827; color: #fff; border: 0; border-radius: 8px; padding: 8px 12px; font-size: 13px; cursor: pointer; }}
    .btn:hover {{ background: #1f2937; }}
    .btn.tiny {{ padding: 3px 8px; font-size: 11px; border-radius: 6px; }}
    .meta {{ color: #334155; font-size: 13px; white-space: nowrap; }}
    .transcript {{ line-height: 1.35; font-size: 13px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }}
    .thumbs {{ display: flex; flex-wrap: nowrap; gap: 6px; flex-shrink: 0; overflow-x: auto; }}
    .thumb {{ display: block; width: 64px; height: 48px; border-radius: 6px; overflow: hidden; border: 1px solid #e2e8f0; background: #f8fafc; flex-shrink: 0; }}
    .thumb img {{ width: 100%; height: 100%; object-fit: cover; display: block; }}
    .thumb-empty {{ color: #64748b; font-size: 12px; white-space: nowrap; }}
    .empty {{ padding: 16px; border: 1px solid #e5e7eb; border-radius: 10px; background: #fff; }}
  </style>
</head>
<body>
  <div class="wrap">
    <h1>Dictation Sessions</h1>
    <p class="sub">Browse and open session reports.</p>
    {content}
  </div>
  <script>
    document.querySelectorAll('.copy-row-transcript').forEach((btn) => {{
      btn.addEventListener('click', async () => {{
        const transcript = btn.dataset.transcript || '';
        if (!navigator.clipboard || !navigator.clipboard.writeText) return;
        try {{
          await navigator.clipboard.writeText(transcript);
          const original = btn.textContent || 'Copy transcript';
          btn.textContent = 'Copied';
          window.setTimeout(() => {{
            btn.textContent = original;
          }}, 1000);
        }} catch (_err) {{}}
      }});
    }});
  </script>
</body>
</html>
"#,
        content = if entries_html.is_empty() {
            "<div class=\"empty\">No sessions found.</div>".to_string()
        } else {
            format!("<div class=\"rows\">{entries_html}</div>")
        }
    )
}

pub(crate) fn shots_from_events(events: &[Value]) -> Vec<ShotMeta> {
    let mut by_id: BTreeMap<usize, ShotMeta> = BTreeMap::new();

    for event in events {
        let etype = event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if etype != "screenshot_moved" && etype != "screenshot_taken" {
            continue;
        }

        let Some(id) = event.get("id").and_then(|v| v.as_u64()).map(|v| v as usize) else {
            continue;
        };
        let Some(dest) = event.get("dest").and_then(|v| v.as_str()) else {
            continue;
        };

        let audio_sec = event
            .get("audioSec")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        let app_name = event
            .get("app")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let app_bundle_id = event
            .get("app")
            .and_then(|v| v.get("bundle_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let app_pid = event
            .get("app")
            .and_then(|v| v.get("pid"))
            .and_then(|v| v.as_i64())
            .and_then(|v| i32::try_from(v).ok());
        let window_title = event
            .get("app")
            .and_then(|v| v.get("window_title"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let app_capture_error = event
            .get("app_capture_error")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let proc_cpu_percent = event
            .get("process")
            .and_then(|v| v.get("cpu_percent"))
            .and_then(|v| v.as_f64());
        let proc_mem_percent = event
            .get("process")
            .and_then(|v| v.get("mem_percent"))
            .and_then(|v| v.as_f64());
        let proc_rss_kb = event
            .get("process")
            .and_then(|v| v.get("rss_kb"))
            .and_then(|v| v.as_u64());
        let proc_elapsed = event
            .get("process")
            .and_then(|v| v.get("elapsed"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let proc_state = event
            .get("process")
            .and_then(|v| v.get("state"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let proc_command = event
            .get("process")
            .and_then(|v| v.get("command"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let proc_capture_error = event
            .get("process_capture_error")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let Some(existing) = by_id.get_mut(&id) {
            existing.dest_rel_path = dest.to_string();
            existing.audio_sec = audio_sec;
            if app_name.is_some() {
                existing.app_name = app_name;
            }
            if app_bundle_id.is_some() {
                existing.app_bundle_id = app_bundle_id;
            }
            if app_pid.is_some() {
                existing.app_pid = app_pid;
            }
            if window_title.is_some() {
                existing.window_title = window_title;
            }
            if app_capture_error.is_some() {
                existing.app_capture_error = app_capture_error;
            }
            if proc_cpu_percent.is_some() {
                existing.proc_cpu_percent = proc_cpu_percent;
            }
            if proc_mem_percent.is_some() {
                existing.proc_mem_percent = proc_mem_percent;
            }
            if proc_rss_kb.is_some() {
                existing.proc_rss_kb = proc_rss_kb;
            }
            if proc_elapsed.is_some() {
                existing.proc_elapsed = proc_elapsed;
            }
            if proc_state.is_some() {
                existing.proc_state = proc_state;
            }
            if proc_command.is_some() {
                existing.proc_command = proc_command;
            }
            if proc_capture_error.is_some() {
                existing.proc_capture_error = proc_capture_error;
            }
            continue;
        }

        by_id.insert(
            id,
            ShotMeta {
                shot_id: id,
                dest_rel_path: dest.to_string(),
                audio_sec,
                app_name,
                app_bundle_id,
                app_pid,
                window_title,
                app_capture_error,
                proc_cpu_percent,
                proc_mem_percent,
                proc_rss_kb,
                proc_elapsed,
                proc_state,
                proc_command,
                proc_capture_error,
            },
        );
    }

    by_id.into_values().collect()
}

pub(crate) fn clipboard_from_events(events: &[Value]) -> Vec<ClipboardMeta> {
    let mut by_id: BTreeMap<usize, ClipboardMeta> = BTreeMap::new();

    for event in events {
        let etype = event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if etype != "clipboard_copied" {
            continue;
        }

        let Some(id) = event.get("id").and_then(|v| v.as_u64()).map(|v| v as usize) else {
            continue;
        };
        let Some(text) = event.get("text").and_then(|v| v.as_str()) else {
            continue;
        };

        let audio_sec = event
            .get("audioSec")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        by_id.insert(
            id,
            ClipboardMeta {
                clip_id: id,
                text: text.to_string(),
                audio_sec,
            },
        );
    }

    by_id.into_values().collect()
}

pub(crate) fn max_shot_id(shots: &[ShotMeta]) -> usize {
    shots.iter().map(|s| s.shot_id).max().unwrap_or(0)
}

pub(crate) fn max_clipboard_id(clips: &[ClipboardMeta]) -> usize {
    clips.iter().map(|c| c.clip_id).max().unwrap_or(0)
}

pub(crate) fn load_shots_for_session(session_dir: &Path, events: &[Value]) -> Vec<ShotMeta> {
    let mut shots = shots_from_events(events);

    if shots.is_empty() {
        let screenshots_dir = session_dir.join("screenshots");
        if let Ok(entries) = fs::read_dir(screenshots_dir) {
            let mut files = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|s| s.to_ascii_lowercase())
                        .map(|ext| SUPPORTED_IMAGE_EXTS.contains(&ext.as_str()))
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>();
            files.sort();
            shots = files
                .iter()
                .enumerate()
                .map(|(i, p)| ShotMeta {
                    shot_id: i + 1,
                    dest_rel_path: format!(
                        "screenshots/{}",
                        p.file_name().and_then(|n| n.to_str()).unwrap_or_default()
                    ),
                    audio_sec: 0.0,
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
                })
                .collect();
        }
    }

    shots.sort_by_key(|s| s.shot_id);
    shots
}

fn session_ended_iso(events: &[Value]) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("session_stopped"))
        .and_then(|e| e.get("ts").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn transcription_meta_from_events(events: &[Value]) -> Value {
    events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("session_stopped"))
        .and_then(|e| e.get("transcription").cloned())
        .unwrap_or_else(|| json!({"status": "unknown"}))
}

pub(crate) fn generate_html_for_session(session_dir: &Path) -> Result<PathBuf, AppError> {
    let session_id = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let events = read_jsonl_values(&session_dir.join("events.jsonl"));
    let started_iso = session_started_iso(&events).unwrap_or_else(|| "unknown".to_string());
    let ended_iso = session_ended_iso(&events).unwrap_or_else(|| "unknown".to_string());
    let audio_duration = session_duration_seconds(&events, session_dir);
    let transcription_meta = transcription_meta_from_events(&events);
    let transcript_fallback = read_transcript_text_for_session(session_dir);
    let shots = load_shots_for_session(session_dir, &events);
    let clips = clipboard_from_events(&events);
    let note_path = session_dir.join("note.md");
    let mut markdown_for_copy = if note_path.exists() {
        fs::read_to_string(&note_path).unwrap_or_default()
    } else {
        String::new()
    };

    let transcript_base = if !markdown_for_copy.trim().is_empty() {
        extract_transcript_from_note(&markdown_for_copy)
            .unwrap_or_else(|| transcript_fallback.clone())
    } else {
        transcript_fallback
    };
    let transcript_base = strip_leading_screenshot_path_block(&transcript_base);
    let transcript_annotated =
        inject_annotation_markers(&transcript_base, &shots, &clips, audio_duration);

    if markdown_for_copy.trim().is_empty() {
        markdown_for_copy = transcript_annotated.clone();
    }

    let html = build_html_note(
        &session_id,
        &started_iso,
        &ended_iso,
        audio_duration,
        &transcription_meta,
        &transcript_annotated,
        &markdown_for_copy,
        &shots,
        &clips,
        session_dir,
        "../index.html",
    );

    let html_path = session_dir.join("note.html");
    fs::write(&html_path, html)
        .map_err(|e| app_error(1, format!("Failed to write {}: {e}", html_path.display())))?;
    Ok(html_path)
}

pub(crate) fn generate_sessions_index_html() -> Result<PathBuf, AppError> {
    let rows = collect_recent_session_rows(5000)?;
    let html = build_sessions_index_html(&rows);
    let html_path = sessions_dir().join("index.html");
    fs::write(&html_path, html)
        .map_err(|e| app_error(1, format!("Failed to write {}: {e}", html_path.display())))?;
    Ok(html_path)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_html() -> String {
        let shots = vec![ShotMeta {
            shot_id: 1,
            dest_rel_path: "screenshots/shot-001.png".to_string(),
            audio_sec: 1.2,
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
        }];
        let clips = vec![];

        build_html_note(
            "20260413-151333",
            "2026-04-13T15:13:33Z",
            "2026-04-13T15:14:33Z",
            Some(60.0),
            &json!({
                "status": "ok",
                "method": "parakeet_server"
            }),
            "hello world",
            "hello world",
            &shots,
            &clips,
            Path::new("/tmp/ispy/sessions/20260413-151333"),
            "../index.html",
        )
    }

    #[test]
    fn html_has_annotate_button_on_screenshot_cards() {
        let html = sample_html();
        assert!(html.contains("annotate-image"));
        assert!(html.contains("data-url=\"screenshots/derived/shot-001__"));
        assert!(html.contains(
            "data-path=\"/tmp/ispy/sessions/20260413-151333/screenshots/derived/shot-001__"
        ));
    }

    #[test]
    fn save_and_close_writes_back_original_image_path() {
        let html = sample_html();
        assert!(html.contains("Save and close"));
        assert!(html.contains("await window.__ispySaveExcalidraw();"));
        assert!(html.contains("closeAnnotator();"));
        assert!(html.contains("absPath: currentContext.path"));
        assert!(html.contains("refreshScreenshotPreview(currentContext.path, currentContext.url);"));
    }

    #[test]
    fn html_includes_excalidraw_ui_container_and_loader() {
        let html = sample_html();
        assert!(html.contains("id=\"annotatorHost\""));
        assert!(html.contains("Loading Excalidraw"));
        assert!(html.contains("window.__ispyOpenExcalidraw"));
        assert!(html.contains("h(pkg.Excalidraw"));
        assert!(html.contains("@excalidraw/excalidraw@0.18.0"));
    }
}
