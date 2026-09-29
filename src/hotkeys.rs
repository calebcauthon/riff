//! `riff hotkeys`: one-command global hotkey setup via skhd.
//!
//! Brew installs the binary but leaves hotkeys as a README exercise. This
//! command closes that gap for the one hotkey daemon that can be driven
//! entirely from a script (Raycast, Alfred, and Keyboard Maestro all need
//! GUI configuration): it writes a clearly marked binding block into the
//! user's skhd config and (re)starts the service.
//!
//! The block is idempotent — install always replaces the previous riff block
//! in place and never touches bindings outside the markers, so users can keep
//! their own skhdrc content around ours.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::json;

use crate::cli::{Cli, HotkeysArgs};
use crate::error::{app_error, AppError};
use crate::{command_exists, emit_json, print_out};

const BLOCK_BEGIN: &str = "# >>> riff hotkeys >>>";
const BLOCK_END: &str = "# <<< riff hotkeys <<<";

pub(crate) fn cmd_hotkeys(cli: &Cli, args: &HotkeysArgs) -> Result<i32, AppError> {
    match args.action.as_str() {
        "install" => install(cli),
        "remove" => remove(cli),
        _ => status(cli),
    }
}

fn skhd_config_path() -> PathBuf {
    // skhd's own lookup order: ~/.skhdrc wins if present, otherwise the XDG
    // location. Mirror it so we edit the file skhd actually reads.
    if let Some(home) = env::var_os("HOME") {
        let legacy = PathBuf::from(&home).join(".skhdrc");
        if legacy.exists() {
            return legacy;
        }
    }
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config"));
    config_home.join("skhd").join("skhdrc")
}

fn riff_binary_path() -> String {
    env::current_exe()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "riff".to_string())
}

fn render_block(riff_bin: &str) -> String {
    // Same defaults the README documents. 0x2C = '/', 0x27 = ''', 0x29 = ';'.
    format!(
        "{BLOCK_BEGIN}\n\
         # Managed by `riff hotkeys install`; edits inside this block are overwritten.\n\
         # alt+/  toggle: start if idle, stop if active\n\
         alt - 0x2C : {riff_bin} --quiet toggle >> /tmp/riff-hotkey.log 2>&1\n\
         # alt+'  toggle + send: stop and paste transcript into the focused app\n\
         alt - 0x27 : {riff_bin} --quiet toggle && {riff_bin} --quiet send >> /tmp/riff-hotkey.log 2>&1\n\
         # alt+;  toggle + open the latest session report\n\
         alt - 0x29 : {riff_bin} --quiet toggle && {riff_bin} --quiet html >> /tmp/riff-hotkey.log 2>&1\n\
         {BLOCK_END}\n"
    )
}

/// Replace the marked riff block in `existing`, or append one. Content
/// outside the markers is preserved byte-for-byte.
pub(crate) fn upsert_block(existing: &str, block: &str) -> String {
    let without = remove_block_text(existing);
    let trimmed = without.trim_end();
    if trimmed.is_empty() {
        block.to_string()
    } else {
        format!("{trimmed}\n\n{block}")
    }
}

/// Strip the marked riff block (markers included) from `existing`.
pub(crate) fn remove_block_text(existing: &str) -> String {
    let Some(begin) = existing.find(BLOCK_BEGIN) else {
        return existing.to_string();
    };
    let after_begin = &existing[begin..];
    let end_rel = after_begin
        .find(BLOCK_END)
        .map(|i| i + BLOCK_END.len())
        .unwrap_or(after_begin.len());
    let mut out = String::new();
    out.push_str(&existing[..begin]);
    let rest = existing[begin + end_rel..].trim_start_matches('\n');
    out.push_str(rest);
    out
}

fn skhd_service(action: &str) -> Result<(), String> {
    let out = Command::new("skhd")
        .arg(format!("--{action}-service"))
        .output()
        .map_err(|e| format!("failed to run skhd --{action}-service: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn skhd_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "skhd"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn install(cli: &Cli) -> Result<i32, AppError> {
    if !command_exists("skhd") {
        print_out(
            cli,
            "skhd is not installed. Install it first, then re-run:\n\n  \
             brew install koekeishiya/formulae/skhd\n  riff hotkeys install"
                .to_string(),
        );
        emit_json(
            cli,
            &json!({"ok": false, "action": "hotkeys_install", "reason": "skhd_not_installed"}),
        );
        return Ok(1);
    }

    let config_path = skhd_config_path();
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| app_error(1, format!("Failed to create {}: {e}", parent.display())))?;
    }
    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    let riff_bin = riff_binary_path();
    let updated = upsert_block(&existing, &render_block(&riff_bin));
    fs::write(&config_path, &updated)
        .map_err(|e| app_error(1, format!("Failed to write {}: {e}", config_path.display())))?;

    // Restart picks up config changes on a running service; fall back to
    // start for first-time installs where no service exists yet.
    let service = skhd_service("restart").or_else(|_| skhd_service("start"));
    let service_note = match &service {
        Ok(()) => "skhd service (re)started.".to_string(),
        Err(e) => format!(
            "Wrote bindings, but starting skhd failed ({e}). Run `skhd --start-service` manually."
        ),
    };

    print_out(
        cli,
        format!(
            "Hotkeys installed in {}:\n  alt+/  toggle recording (start/stop)\n  alt+'  \
             toggle + paste transcript into the focused app\n  alt+;  toggle + open the HTML report\n\
             {service_note}\n\
             First run only: macOS will ask to grant skhd Accessibility access\n\
             (System Settings -> Privacy & Security -> Accessibility).",
            config_path.display()
        ),
    );
    emit_json(
        cli,
        &json!({
            "ok": service.is_ok(),
            "action": "hotkeys_install",
            "config": config_path.display().to_string(),
            "riff_bin": riff_bin,
            "service_started": service.is_ok(),
        }),
    );
    Ok(0)
}

fn remove(cli: &Cli) -> Result<i32, AppError> {
    let config_path = skhd_config_path();
    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    if !existing.contains(BLOCK_BEGIN) {
        print_out(
            cli,
            "No riff hotkey block found; nothing to remove.".to_string(),
        );
        emit_json(
            cli,
            &json!({"ok": true, "action": "hotkeys_remove", "removed": false}),
        );
        return Ok(0);
    }
    let updated = remove_block_text(&existing);
    fs::write(&config_path, &updated)
        .map_err(|e| app_error(1, format!("Failed to write {}: {e}", config_path.display())))?;
    if command_exists("skhd") && skhd_running() {
        let _ = skhd_service("restart");
    }
    print_out(
        cli,
        format!("Removed riff hotkeys from {}.", config_path.display()),
    );
    emit_json(
        cli,
        &json!({"ok": true, "action": "hotkeys_remove", "removed": true}),
    );
    Ok(0)
}

fn status(cli: &Cli) -> Result<i32, AppError> {
    let config_path = skhd_config_path();
    let installed = command_exists("skhd");
    let running = installed && skhd_running();
    let block_present = fs::read_to_string(&config_path)
        .map(|c| c.contains(BLOCK_BEGIN))
        .unwrap_or(false);

    print_out(
        cli,
        format!(
            "skhd installed: {installed}\nskhd running: {running}\nconfig: {}\nriff bindings present: {block_present}\n\n{}",
            config_path.display(),
            if block_present {
                "Hotkeys: alt+/ toggle, alt+' toggle+send, alt+; toggle+html"
            } else {
                "Run `riff hotkeys install` to set up global hotkeys."
            }
        ),
    );
    emit_json(
        cli,
        &json!({
            "ok": true,
            "action": "hotkeys_status",
            "skhd_installed": installed,
            "skhd_running": running,
            "config": config_path.display().to_string(),
            "bindings_present": block_present,
        }),
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_appends_to_existing_content() {
        let existing = "# my bindings\ncmd - a : open -a Safari\n";
        let out = upsert_block(existing, &render_block("/usr/local/bin/riff"));
        assert!(out.starts_with("# my bindings\ncmd - a : open -a Safari\n\n# >>> riff"));
        assert!(out.ends_with(&format!("{BLOCK_END}\n")));
    }

    #[test]
    fn upsert_replaces_previous_block_in_place() {
        let first = upsert_block("cmd - a : true\n", &render_block("/old/riff"));
        let second = upsert_block(&first, &render_block("/new/riff"));
        assert_eq!(second.matches(BLOCK_BEGIN).count(), 1);
        assert!(second.contains("/new/riff"));
        assert!(!second.contains("/old/riff"));
        assert!(second.contains("cmd - a : true"));
    }

    #[test]
    fn remove_preserves_everything_outside_markers() {
        let content = upsert_block("before\n", &render_block("/bin/riff")) + "\nafter\n";
        let out = remove_block_text(&content);
        assert!(!out.contains(BLOCK_BEGIN));
        assert!(!out.contains("/bin/riff"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn remove_on_clean_config_is_identity() {
        let content = "cmd - a : true\n";
        assert_eq!(remove_block_text(content), content);
    }
}
