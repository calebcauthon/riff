use crate::error::{app_error, AppError};
use std::env;
use std::fs;
use std::path::PathBuf;

pub fn root_dir() -> PathBuf {
    env::var("RIFF_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/riff"))
}

pub fn sessions_dir() -> PathBuf {
    root_dir().join("sessions")
}

pub fn active_state_file() -> PathBuf {
    root_dir().join("active_session.json")
}

pub fn last_session_file() -> PathBuf {
    root_dir().join("last_session.json")
}

pub fn perf_log_file() -> PathBuf {
    root_dir().join("perf.jsonl")
}

pub fn audio_device_cache_file() -> PathBuf {
    root_dir().join("audio_device_cache.txt")
}

pub fn watcher_python_cache_file() -> PathBuf {
    root_dir().join("watcher_python_cache.txt")
}

pub fn parakeet_server_log_file() -> PathBuf {
    root_dir().join("parakeet-server.log")
}

pub fn parakeet_server_pid_file() -> PathBuf {
    root_dir().join("parakeet-server.pid")
}

pub fn web_server_log_file() -> PathBuf {
    root_dir().join("web-server.log")
}

pub fn web_server_pid_file() -> PathBuf {
    root_dir().join("web-server.pid")
}

pub fn ensure_dirs() -> Result<(), AppError> {
    fs::create_dir_all(root_dir())
        .and_then(|_| fs::create_dir_all(sessions_dir()))
        .map_err(|e| app_error(1, format!("Failed to create app dirs: {e}")))
}
