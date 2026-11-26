use std::path::{Path, PathBuf};
use std::process::Command;

/// Config every git process shipcheck starts gets, since git checks out a branch's tree on the host
/// with the user's global config. LFS read `.lfsconfig` from the tree and has written through its
/// symlinks, and submodules ran hooks (CVE-2025-48384). Hooks themselves come from the user's own
/// `.git`, never from the tree, and `shipcheck install` reads `core.hooksPath`, so it stays
const SAFE_GIT: [(&str, &str); 6] = [
    ("filter.lfs.process", ""),
    ("filter.lfs.smudge", ""),
    ("filter.lfs.clean", ""),
    ("filter.lfs.required", "false"),
    ("submodule.recurse", "false"),
    ("core.fsmonitor", "false"),
];

/// The `GIT_CONFIG_*` variables for `SAFE_GIT`, after the `existing` entries the user already set
pub fn safe_git_env(existing: usize) -> Vec<(String, String)> {
    let mut vars = vec![(
        "GIT_CONFIG_COUNT".to_string(),
        (existing + SAFE_GIT.len()).to_string(),
    )];

    for (i, (key, value)) in SAFE_GIT.iter().enumerate() {
        vars.push((format!("GIT_CONFIG_KEY_{}", existing + i), key.to_string()));
        vars.push((
            format!("GIT_CONFIG_VALUE_{}", existing + i),
            value.to_string(),
        ));
    }

    vars
}

pub(crate) fn git_raw(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    Ok(out.stdout)
}

pub(crate) fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&git_raw(repo, args)?)
        .trim()
        .to_string())
}

pub fn repo_id(repo: &Path) -> Result<String, String> {
    let url = git(repo, &["remote", "get-url", "origin"])?;

    if url.starts_with('.') {
        let abs = repo
            .join(&url)
            .canonicalize()
            .map_err(|e| format!("origin {url}: {e}"))?;
        return parse_repo_id(&abs.to_string_lossy());
    }

    parse_repo_id(&url)
}

/// `owner/repo` from an origin URL, for ssh, https and local path remotes alike
fn parse_repo_id(url: &str) -> Result<String, String> {
    let url = url.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = url
        .rsplit(['/', ':'])
        .filter(|p| !p.is_empty() && *p != "." && *p != "..");
    match (parts.next(), parts.next()) {
        (Some(name), Some(owner)) => Ok(format!("{owner}/{name}")),
        _ => Err(format!("cannot read owner/repo from origin {url}")),
    }
}

/// The checkout the daemon fetches into: the main one, not a worktree that may go away
pub(crate) fn main_checkout(root: &Path) -> PathBuf {
    Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(root)
        .output()
        .ok()
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .filter(|dir| dir.file_name().is_some_and(|n| n == ".git"))
        .and_then(|dir| dir.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_from_every_remote_form() {
        for url in [
            "git@github.com:acme/app.git",
            "https://github.com/acme/app.git",
            "https://github.com/acme/app",
            "ssh://git@github.com/acme/app.git/",
            "/tmp/x/acme/app.git",
        ] {
            assert_eq!(parse_repo_id(url).unwrap(), "acme/app", "{url}");
        }

        assert!(parse_repo_id("app").is_err());
        assert!(
            parse_repo_id("../app.git").is_err(),
            "must not climb out of the state directory"
        );
    }

    #[test]
    fn more_remote_forms() {
        assert_eq!(
            parse_repo_id("https://github.com:443/Acme/App.git").unwrap(),
            "Acme/App"
        );
        assert_eq!(
            parse_repo_id("git@github.com:acme/app").unwrap(),
            "acme/app"
        );
        assert_eq!(
            parse_repo_id("file:///srv/git/acme/app.git").unwrap(),
            "acme/app"
        );
        assert!(parse_repo_id("").is_err());
        assert!(parse_repo_id("/").is_err());
    }
}
