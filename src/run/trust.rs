//! Repositories whose `sandbox = false` host checks this machine lets run outside the sandbox

use crate::paths;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn file() -> PathBuf {
    paths::home().join("trusted")
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

/// Adds this repository's origin, and returns it
pub fn add(repo: &Path) -> Result<String, String> {
    let origin = origin(repo)?;
    let mut list = others(&origin);
    list.push(origin.clone());
    save(&list)?;
    Ok(origin)
}

/// Removes this repository's origin, and returns it
pub fn remove(repo: &Path) -> Result<String, String> {
    let origin = origin(repo)?;
    save(&others(&origin))?;
    Ok(origin)
}

fn others(origin: &str) -> Vec<String> {
    listed().into_iter().filter(|o| o != origin).collect()
}

fn save(list: &[String]) -> Result<(), String> {
    fs::create_dir_all(paths::home()).map_err(|e| e.to_string())?;
    let text = list.iter().fold(String::new(), |mut s, o| {
        let _ = writeln!(s, "{o}");
        s
    });
    fs::write(file(), text).map_err(|e| e.to_string())
}
