//! Repositories whose `sandbox = false` host checks this machine lets run outside the sandbox

use std::fs;
use std::path::{Path, PathBuf};

fn file() -> PathBuf {
    crate::run::home().join("trusted")
}

/// The origin URL exactly as git has it, so a clone whose path ends in the same owner/name is not the same repository
fn origin(repo: &Path) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .output()
        .map_err(|e| format!("git: {e}"))?;

    if !out.status.success() {
        return Err("this repository has no origin remote to trust".into());
    }

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn listed() -> Vec<String> {
    fs::read_to_string(file())
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Whether `shipcheck trust` ran for this repository's origin on this machine
pub fn trusted(repo: &Path) -> bool {
    origin(repo).is_ok_and(|o| listed().contains(&o))
}

/// Adds or removes this repository's origin, and returns it
pub fn set(repo: &Path, on: bool) -> Result<String, String> {
    let origin = origin(repo)?;
    let mut list: Vec<String> = listed().into_iter().filter(|o| *o != origin).collect();

    if on {
        list.push(origin.clone());
    }

    fs::create_dir_all(crate::run::home()).map_err(|e| e.to_string())?;
    let text: String = list.iter().map(|o| format!("{o}\n")).collect();
    fs::write(file(), text).map_err(|e| e.to_string())?;
    Ok(origin)
}
