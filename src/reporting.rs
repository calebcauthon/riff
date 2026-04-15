use crate::error::{app_error, AppError};
use crate::history::{
    collect_recent_session_rows, extract_transcript_from_note, format_duration_compact,
    read_jsonl_values, read_transcript_text_for_session, session_duration_seconds,
    session_started_iso,
};
use crate::models::{ClipboardMeta, SessionListRow, SessionState, ShotMeta};
use crate::paths::sessions_dir;
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
    let clean = transcript.trim();
    let mut markers = shots
        .iter()
        .map(|s| (s.audio_sec, format!("[Screenshot {}]", s.shot_id)))
        .chain(
            clips
                .iter()
                .map(|c| (c.audio_sec, format!("[Clipboard {}]", c.clip_id))),
        )
        .collect::<Vec<_>>();
    markers.retain(|(_, marker)| !clean.contains(marker));
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
        if clean.contains(&marker) {
            continue;
        }
        let ratio = (audio_sec / duration).clamp(0.0, 1.0);
        let mut idx = ((base_len as f64) * ratio).round() as usize;
        idx = idx.min(tokens.len());
        let insert_at = (idx + inserted).min(tokens.len());
        tokens.insert(insert_at, marker);
        inserted += 1;
    }

    tokens.join(" ")
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    #[test]
    fn inject_annotation_markers_does_not_duplicate_existing_screenshot_marker() {
        let transcript =
            "Screenshot 1: /tmp/ispy/sessions/abc/screenshots/shot-001.png Testing, testing, [Screenshot 1] testing.";
        let shots = vec![ShotMeta {
            shot_id: 1,
            dest_rel_path: "screenshots/shot-001.png".to_string(),
            audio_sec: 1.0,
        }];

        let out = inject_annotation_markers(transcript, &shots, &[], Some(3.0));
        assert_eq!(out, transcript);
        assert!(!out.contains("[Screenshot [Screenshot 1] 1]"));
    }
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

    lines.push(String::new());
    lines.push("## Transcript".to_string());
    lines.push(String::new());

    if !shots.is_empty() {
        let session_dir = Path::new(&state.session_dir);
        for shot in shots {
            let abs_path = session_dir.join(&shot.dest_rel_path);
            lines.push(format!(
                "Screenshot {}: {}",
                shot.shot_id,
                abs_path.display()
            ));
        }
        lines.push(String::new());
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
            lines.push(format!(
                "[Screenshot {}]: {} (t={})",
                shot.shot_id,
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
        gallery.push_str(&format!(
            r#"<figure class="card"><div class="card-head"><figcaption>Screenshot {}</figcaption><div class="card-actions"><button class="btn small annotate-image" data-url="{}" data-path="{}">Annotate</button><button class="btn small copy-image" data-url="{}" data-path="{}">Copy image</button></div></div><a href="{}" target="_blank" rel="noreferrer"><img src="{}" alt="Screenshot {}" loading="lazy" /></a><div class="path">{}</div></figure>"#,
            shot.shot_id,
            html_escape(&rel_url),
            html_escape(&abs_str),
            html_escape(&rel_url),
            html_escape(&abs_str),
            html_escape(&rel_url),
            html_escape(&rel_url),
            shot.shot_id,
            html_escape(&abs_str)
        ));
    }

    let mut clip_cards = String::new();
    for clip in clips {
        let preview = clip_preview(&clip.text, 300);
        clip_cards.push_str(&format!(
            r#"<div class="card"><div class="card-head"><strong>Clipboard {}</strong><button class="btn small copy-clip" data-text="{}">Copy text</button></div><div class="transcript">{}</div><div class="path">t={} · {} chars</div></div>"#,
            clip.clip_id,
            html_escape(&clip.text),
            html_escape(&preview),
            html_escape(&format_hms(clip.audio_sec)),
            clip.text.chars().count()
        ));
    }

    let duration = format_duration_compact(audio_duration_sec);
    let transcript_text = if transcript.trim().is_empty() {
        "_No transcript available._".to_string()
    } else {
        transcript.trim().to_string()
    };

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Dictation {session_id}</title>
  <link rel="stylesheet" href="https://esm.sh/@excalidraw/excalidraw@0.18.0/dist/dev/index.css" />
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
    .btn {{ background: #111827; color: #fff; border: 0; border-radius: 8px; padding: 8px 12px; font-size: 13px; cursor: pointer; }}
    .btn:hover {{ background: #1f2937; }}
    .btn.small {{ padding: 6px 10px; font-size: 12px; }}
    .btn.tiny {{ padding: 3px 8px; font-size: 11px; border-radius: 6px; }}
    .transcript {{ white-space: pre-wrap; line-height: 1.6; font-size: 15px; }}
    .grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(280px, 1fr)); gap: 12px; }}
    .card {{ background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 10px; margin: 0; }}
    .card-head {{ display: flex; justify-content: space-between; align-items: center; gap: 8px; margin-bottom: 8px; }}
    .card-actions {{ display: flex; align-items: center; gap: 6px; }}
    .card img {{ width: 100%; height: auto; border-radius: 8px; display: block; }}
    .card figcaption {{ font-weight: 600; margin: 0; }}
    .path {{ color: #6b7280; margin-top: 8px; font-size: 12px; word-break: break-all; }}
    .annotator-modal {{ position: fixed; inset: 0; background: rgba(15, 23, 42, 0.72); display: none; align-items: center; justify-content: center; padding: 16px; z-index: 9999; }}
    .annotator-modal.open {{ display: flex; }}
    .annotator-panel {{ position: relative; width: min(1200px, 100%); max-height: calc(100vh - 32px); overflow: auto; background: #fff; border-radius: 12px; border: 1px solid #e5e7eb; padding: 12px; }}
    .annotator-toolbar {{ display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-bottom: 10px; }}
    .annotator-close-x {{ position: absolute; top: 10px; right: 10px; width: 32px; height: 32px; border: 1px solid #cbd5e1; border-radius: 8px; background: #fff; color: #111827; font-size: 20px; line-height: 1; cursor: pointer; }}
    .annotator-close-x:hover {{ background: #f1f5f9; }}
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
        <button id="copyMarkdownBtn" class="btn">Copy markdown</button>
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
        <button id="copyTranscriptBtn" class="btn tiny">Copy</button>
      </div>
      <div class="transcript">{transcript_html}</div>
    </section>

    <section class="panel">
      <h2>Screenshots</h2>
      {gallery_html}
    </section>

    <section class="panel">
      <h2>Clipboard</h2>
      {clipboard_html}
    </section>
  </div>
  <div id="annotatorModal" class="annotator-modal" aria-hidden="true">
    <div class="annotator-panel">
      <button id="annotatorCloseBtn" class="annotator-close-x" aria-label="Close annotation dialog">&times;</button>
      <div class="annotator-toolbar">
        <button id="annotatorSaveBtn" class="btn small">Save &amp; close</button>
        <button id="annotatorDownloadBtn" class="btn small">Download annotated PNG</button>
        <span id="annotatorStatus" class="status"></span>
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
    const annotatorLoading = document.getElementById('annotatorLoading');
    let currentAnnotatorPath = '';

    function setAnnotatorStatus(message) {{
      if (!annotatorStatus) return;
      annotatorStatus.textContent = message;
      window.setTimeout(() => {{
        if (annotatorStatus.textContent === message) annotatorStatus.textContent = '';
      }}, 2200);
    }}

    function openAnnotator(url, path) {{
      if (!annotatorModal) return;
      currentAnnotatorPath = path || url;
      annotatorModal.classList.add('open');
      annotatorModal.setAttribute('aria-hidden', 'false');
      annotatorLoading?.classList.remove('hidden');
      if (typeof window.__ispyOpenExcalidraw === 'function') {{
        window.__ispyOpenExcalidraw(url, currentAnnotatorPath);
      }}
    }}

    function closeAnnotator() {{
      if (!annotatorModal) return;
      annotatorModal.classList.remove('open');
      annotatorModal.setAttribute('aria-hidden', 'true');
      currentAnnotatorPath = '';
    }}

    document.getElementById('annotatorCloseBtn')?.addEventListener('click', closeAnnotator);
    annotatorModal?.addEventListener('click', (evt) => {{
      if (evt.target === annotatorModal) closeAnnotator();
    }});
    window.addEventListener('keydown', (evt) => {{
      if (evt.key === 'Escape' && annotatorModal?.classList.contains('open')) closeAnnotator();
    }});

    document.querySelectorAll('.annotate-image').forEach((btn) => {{
      btn.addEventListener('click', () => {{
        const url = btn.dataset.url || '';
        const path = btn.dataset.path || url;
        if (!url) {{
          setAnnotatorStatus('Missing image URL');
          return;
        }}
        openAnnotator(url, path);
      }});
    }});

    document.getElementById('annotatorSaveBtn')?.addEventListener('click', async () => {{
      if (typeof window.__ispySaveExcalidraw !== 'function') {{
        setAnnotatorStatus('Excalidraw not ready');
        return;
      }}
      try {{
        await window.__ispySaveExcalidraw();
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
        const card = btn.closest('.card');
        if (!card) return;
        card.querySelectorAll('button[data-path]').forEach((cardBtn) => {{
          cardBtn.dataset.url = baseUrl;
        }});
        const img = card.querySelector('img');
        const link = card.querySelector('a');
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
          // Rewrite old payloads so subsequent opens don't hit bad historical state.
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
"##,
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
            format!("<div class=\"grid\">{}</div>", gallery)
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
        let note_path = session_dir.join("note.md");
        let transcript_raw = fs::read_to_string(&note_path)
            .ok()
            .and_then(|markdown| extract_transcript_from_note(&markdown))
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| read_transcript_text_for_session(&session_dir));
        let transcript_copy = if transcript_raw.trim().is_empty() {
            "No transcript available.".to_string()
        } else {
            transcript_raw.trim().to_string()
        };
        let transcript = if transcript_raw.trim().is_empty() {
            "No transcript available.".to_string()
        } else {
            transcript_raw
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
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

        entries_html.push_str(&format!(
            r#"<article class="row" data-href="./{session_id}/note.html" tabindex="0"><div class="main"><div class="row-top"><span class="session">{session_id}</span><span class="meta">{timestamp}</span><span class="meta">{images} images</span><span class="meta">{duration}</span><button class="btn tiny copy-row-transcript" data-transcript="{transcript_copy}" title="Copy transcript">Copy</button></div><div class="transcript" title="{transcript_title}">{transcript}</div></div><div class="thumbs">{thumbs}</div></article>"#,
            session_id = html_escape(&row.session_id),
            timestamp = html_escape(&row.timestamp),
            images = row.images,
            duration = html_escape(&row.duration),
            transcript = html_escape(&transcript),
            transcript_copy = html_escape(&transcript_copy),
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
    .row {{ background: #fff; border: 1px solid #e5e7eb; border-radius: 12px; padding: 10px 12px; display: flex; align-items: center; gap: 12px; cursor: pointer; }}
    .row:hover {{ border-color: #cbd5e1; box-shadow: 0 2px 8px rgba(2, 6, 23, 0.08); }}
    .row:focus-visible {{ outline: 2px solid #2563eb; outline-offset: 2px; }}
    .main {{ min-width: 0; flex: 1; }}
    .row-top {{ display: flex; flex-wrap: nowrap; align-items: center; gap: 10px; margin-bottom: 4px; overflow: hidden; }}
    .session {{ color: #1d4ed8; font-weight: 700; white-space: nowrap; }}
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
    async function copyText(text) {{
      if (!navigator.clipboard || !navigator.clipboard.writeText) {{
        throw new Error('Clipboard unavailable');
      }}
      await navigator.clipboard.writeText(text);
    }}

    document.querySelectorAll('.row').forEach((row) => {{
      row.addEventListener('click', (event) => {{
        if (event.target.closest('.copy-row-transcript') || event.target.closest('.thumb')) {{
          return;
        }}
        const href = row.dataset.href;
        if (href) window.location.href = href;
      }});

      row.addEventListener('keydown', (event) => {{
        if (event.key !== 'Enter' && event.key !== ' ') return;
        const href = row.dataset.href;
        if (!href) return;
        event.preventDefault();
        window.location.href = href;
      }});
    }});

    document.querySelectorAll('.copy-row-transcript').forEach((btn) => {{
      btn.addEventListener('click', async (event) => {{
        event.preventDefault();
        event.stopPropagation();
        const text = btn.dataset.transcript || '';
        const original = btn.textContent || 'Copy';
        try {{
          await copyText(text);
          btn.textContent = 'Copied';
          window.setTimeout(() => {{
            btn.textContent = original;
          }}, 900);
        }} catch (_err) {{
          btn.textContent = 'Failed';
          window.setTimeout(() => {{
            btn.textContent = original;
          }}, 900);
        }}
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

        by_id.insert(
            id,
            ShotMeta {
                shot_id: id,
                dest_rel_path: dest.to_string(),
                audio_sec,
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
        assert!(html.contains("data-url=\"screenshots/shot-001.png\""));
        assert!(html
            .contains("data-path=\"/tmp/ispy/sessions/20260413-151333/screenshots/shot-001.png\""));
    }

    #[test]
    fn save_and_close_writes_back_original_image_path() {
        let html = sample_html();
        assert!(html.contains("Save &amp; close"));
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
