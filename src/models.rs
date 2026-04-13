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
}

#[derive(Debug, Clone)]
pub struct ShotMeta {
    pub shot_id: usize,
    pub dest_rel_path: String,
    pub audio_sec: f64,
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
