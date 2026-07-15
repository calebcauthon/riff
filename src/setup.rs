use crate::cli::{Cli, DoctorArgs, SetupArgs};
use crate::error::{app_error, AppError};
use crate::paths::{root_dir, sessions_dir};
use crate::transcription::{
    check_parakeet_server_health, check_web_server_health, default_parakeet_script,
    default_sound_picker_script, default_web_server_script, parakeet_server_base_url,
    resolve_python_bin, resource_dir, web_server_base_url,
};
use crate::{command_exists, emit_json, print_out};
use serde_json::json;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) const PINNED_PARAKEET_MODEL: &str =
    "nvidia/stt_en_fastconformer_hybrid_medium_streaming_80ms_pc";
pub(crate) const PINNED_PARAKEET_MODEL_REVISION: &str = "main";
pub(crate) const PARAKEET_REQUIREMENTS_REL: &str = "scripts/parakeet-requirements.txt";

pub(crate) fn default_user_runtime_dir() -> PathBuf {
    if let Some(path) = env::var_os("RIFF_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("riff")
            .join("runtime")
            .join("python");
    }
    root_dir().join("runtime").join("python")
}

fn runtime_python(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("bin").join("python")
}

fn command_status_ok(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn python_version_ok(python: &str) -> bool {
    Command::new(python)
        .arg("-c")
        .arg("import sys; raise SystemExit(0 if sys.version_info[:2] == (3, 12) else 1)")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn requirements_path() -> Option<PathBuf> {
    resource_dir()
        .map(|dir| dir.join(PARAKEET_REQUIREMENTS_REL))
        .filter(|p| p.exists())
}

fn check_path(label: &str, path: Option<PathBuf>, rows: &mut Vec<(String, bool, String)>) {
    match path {
        Some(path) => rows.push((label.to_string(), true, path.display().to_string())),
        None => rows.push((label.to_string(), false, "not found".to_string())),
    }
}

fn writable_dir(path: &Path) -> bool {
    fs::create_dir_all(path).is_ok()
        && fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path.join(".riff-write-test"))
            .and_then(|_| fs::remove_file(path.join(".riff-write-test")))
            .is_ok()
}

pub(crate) fn cmd_setup(cli: &Cli, args: &SetupArgs) -> Result<i32, AppError> {
    let runtime_dir = args
        .runtime_dir
        .clone()
        .unwrap_or_else(default_user_runtime_dir);
    let python = args
        .python
        .clone()
        .or_else(|| {
            if command_exists("python3.12") {
                Some("python3.12".to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "python3".to_string());

    if !python_version_ok(&python) {
        return Err(app_error(
            1,
            format!("{python} is not Python 3.12. Install python@3.12 or pass --python."),
        ));
    }

    if cli.dry_run {
        print_out(
            cli,
            format!(
                "[dry-run] Would create runtime at {} with {}",
                runtime_dir.display(),
                python
            ),
        );
        return Ok(0);
    }

    if !runtime_python(&runtime_dir).exists() {
        if let Some(parent) = runtime_dir.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| app_error(1, format!("Failed to create {}: {e}", parent.display())))?;
        }
        print_out(
            cli,
            format!("Creating private Python runtime: {}", runtime_dir.display()),
        );
        let status = Command::new(&python)
            .args(["-m", "venv"])
            .arg(&runtime_dir)
            .status()
            .map_err(|e| app_error(1, format!("Failed to run {python}: {e}")))?;
        if !status.success() {
            return Err(app_error(1, "Failed to create private Python runtime."));
        }
    }

    let runtime_py = runtime_python(&runtime_dir);
    if !args.skip_packages {
        let requirements = requirements_path().ok_or_else(|| {
            app_error(
                1,
                format!("Could not find {PARAKEET_REQUIREMENTS_REL} in riff resources."),
            )
        })?;
        print_out(
            cli,
            format!(
                "Installing pinned transcription packages from {}",
                requirements.display()
            ),
        );
        let status = Command::new(&runtime_py)
            .args(["-m", "pip", "install", "--upgrade", "pip"])
            .status()
            .map_err(|e| app_error(1, format!("Failed to run pip: {e}")))?;
        if !status.success() {
            return Err(app_error(1, "Failed to upgrade pip in private runtime."));
        }
        let status = Command::new(&runtime_py)
            .args(["-m", "pip", "install", "-r"])
            .arg(&requirements)
            .status()
            .map_err(|e| app_error(1, format!("Failed to install packages: {e}")))?;
        if !status.success() {
            return Err(app_error(
                1,
                "Failed to install pinned transcription packages.",
            ));
        }
    }

    if !args.skip_model {
        let Some(script) = default_parakeet_script() else {
            return Err(app_error(1, "Could not find Parakeet helper script."));
        };
        print_out(
            cli,
            format!(
                "Downloading model {} (revision {}) with visible upstream progress",
                PINNED_PARAKEET_MODEL, PINNED_PARAKEET_MODEL_REVISION
            ),
        );
        let status = Command::new(&runtime_py)
            .arg(script)
            .arg("--download-model")
            .arg("--model")
            .arg(PINNED_PARAKEET_MODEL)
            .env(
                "RIFF_PARAKEET_MODEL_REVISION",
                PINNED_PARAKEET_MODEL_REVISION,
            )
            .status()
            .map_err(|e| app_error(1, format!("Failed to pre-download model: {e}")))?;
        if !status.success() {
            return Err(app_error(1, "Model pre-download failed."));
        }
    }

    print_out(
        cli,
        format!(
            "Setup complete.\nRIFF_RUNTIME_DIR={}\npython={}",
            runtime_dir.display(),
            runtime_py.display()
        ),
    );
    emit_json(
        cli,
        &json!({
            "ok": true,
            "runtime_dir": runtime_dir,
            "python": runtime_py,
            "model": PINNED_PARAKEET_MODEL,
            "model_revision": PINNED_PARAKEET_MODEL_REVISION
        }),
    );
    Ok(0)
}

pub(crate) fn cmd_doctor(cli: &Cli, args: &DoctorArgs) -> Result<i32, AppError> {
    let mut rows: Vec<(String, bool, String)> = Vec::new();
    rows.push((
        "resource_dir".to_string(),
        resource_dir().is_some(),
        resource_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not found".to_string()),
    ));
    check_path("parakeet_script", default_parakeet_script(), &mut rows);
    check_path("web_server_script", default_web_server_script(), &mut rows);
    check_path(
        "sound_picker_script",
        default_sound_picker_script(),
        &mut rows,
    );
    check_path("requirements", requirements_path(), &mut rows);

    rows.push((
        "ffmpeg".to_string(),
        command_exists("ffmpeg"),
        if command_exists("ffmpeg") {
            "found"
        } else {
            "missing"
        }
        .to_string(),
    ));
    rows.push((
        "screencapture".to_string(),
        command_exists("screencapture"),
        if command_exists("screencapture") {
            "found"
        } else {
            "missing"
        }
        .to_string(),
    ));
    rows.push((
        "osascript/accessibility".to_string(),
        command_exists("osascript"),
        if command_exists("osascript") {
            "osascript found; grant Accessibility when macOS prompts"
        } else {
            "osascript missing"
        }
        .to_string(),
    ));

    let python = resolve_python_bin(None);
    rows.push((
        "python".to_string(),
        command_exists(&python),
        python.clone(),
    ));
    if args.deep {
        let imports_ok = command_status_ok(
            Command::new(&python)
                .arg("-c")
                .arg("import torch, soundfile; import nemo.collections.asr.models"),
        );
        rows.push((
            "python_packages".to_string(),
            imports_ok,
            if imports_ok {
                "torch, soundfile, nemo import"
            } else {
                "missing import; run riff setup"
            }
            .to_string(),
        ));
    }

    rows.push((
        "root_storage".to_string(),
        writable_dir(&root_dir()),
        root_dir().display().to_string(),
    ));
    rows.push((
        "sessions_storage".to_string(),
        writable_dir(&sessions_dir()),
        sessions_dir().display().to_string(),
    ));
    let parakeet_url = parakeet_server_base_url();
    rows.push((
        "parakeet_server".to_string(),
        check_parakeet_server_health(&parakeet_url),
        parakeet_url,
    ));
    let web_url = web_server_base_url();
    rows.push((
        "web_server".to_string(),
        check_web_server_health(&web_url),
        web_url,
    ));

    let ok = rows.iter().all(|(name, status, _)| {
        *status || matches!(name.as_str(), "parakeet_server" | "web_server")
    });
    for (label, status, detail) in &rows {
        print_out(
            cli,
            format!(
                "{:<24} {:<4} {}",
                label,
                if *status { "ok" } else { "fail" },
                detail
            ),
        );
    }
    emit_json(
        cli,
        &json!({
            "ok": ok,
            "checks": rows.iter().map(|(name, ok, detail)| json!({
                "name": name,
                "ok": ok,
                "detail": detail
            })).collect::<Vec<_>>()
        }),
    );
    Ok(if ok { 0 } else { 1 })
}
