use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub session_dir: String,
    pub screenshots_dir: String,
    pub audio_path: String,
    pub events_path: String,
    pub ffmpeg_log_path: String,
    pub ffmpeg_pid: Option<i32>,
    pub started_at_iso: String,
    pub started_at_epoch: f64,
    pub screenshot_source_dir: String,
    pub audio_device: String,
    #[serde(default)]
    pub clipboard_watcher_pid: Option<i32>,
    #[serde(default)]
    pub transcription_watcher_pid: Option<i32>,
    #[serde(default)]
    pub transcription_cursor_sec: f64,
    #[serde(default)]
    pub transcription_paused: bool,
    #[serde(default)]
    pub transcription_pause_started_sec: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ShotMeta {
    pub shot_id: usize,
    pub dest_rel_path: String,
    pub audio_sec: f64,
    pub app_name: Option<String>,
    pub app_bundle_id: Option<String>,
    pub app_pid: Option<i32>,
    pub window_title: Option<String>,
    pub app_capture_error: Option<String>,
    pub proc_cpu_percent: Option<f64>,
    pub proc_mem_percent: Option<f64>,
    pub proc_rss_kb: Option<u64>,
    pub proc_elapsed: Option<String>,
    pub proc_state: Option<String>,
    pub proc_command: Option<String>,
    pub proc_capture_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClipboardMeta {
    pub clip_id: usize,
    pub text: String,
    pub audio_sec: f64,
}

#[derive(Debug, Serialize)]
pub struct SessionListRow {
    pub session_id: String,
    pub timestamp: String,
    pub summary: String,
    pub images: usize,
    pub duration: String,
}
