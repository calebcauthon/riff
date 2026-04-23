use std::process::Command;

fn git_stdout(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=RIFF_BUILD_ID");

    if let Ok(explicit) = std::env::var("RIFF_BUILD_ID") {
        let val = explicit.trim();
        if !val.is_empty() {
            println!("cargo:rustc-env=RIFF_BUILD_ID={val}");
            println!("cargo:warning=riff build id: {val}");
            return;
        }
    }

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let sha =
        git_stdout(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "nogit".to_string());
    let dirty = git_stdout(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let suffix = if dirty { "-dirty" } else { "" };
    let build_id = format!("{version}+{sha}{suffix}");
    println!("cargo:rustc-env=RIFF_BUILD_ID={build_id}");
    println!("cargo:warning=riff build id: {build_id}");
}
