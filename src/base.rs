use crate::daemon_config;
use crate::git;
use crate::git::git;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::daemon_config::{DaemonConfig, RepoEntry, origin_main};

pub fn remote_base(repo: &Path) -> Option<String> {
    remote_head(repo).map(|(branch, _)| format!("origin/{branch}"))
}

/// The commit the remote's default branch is at, which no local ref can move
pub fn remote_base_sha(repo: &Path) -> Option<String> {
    remote_head(repo).map(|(_, sha)| sha)
}

/// The remote's default branch and its commit, asked once per process for each checkout and URL
fn remote_head(repo: &Path) -> Option<(String, String)> {
    type Asked = BTreeMap<(PathBuf, String), Option<(String, String)>>;
    static ASKED: Mutex<Asked> = Mutex::new(BTreeMap::new());
    let url = git(repo, &["remote", "get-url", "origin"]).unwrap_or_default();
    let mut asked = ASKED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    asked
        .entry((repo.to_path_buf(), url))
        .or_insert_with(|| remote_default(repo))
        .clone()
}

/// `git ls-remote --symref origin HEAD`'s branch and commit, None when the remote does not answer in 5s
fn remote_default(repo: &Path) -> Option<(String, String)> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(daemon_config::fetch_options())
        .args(["ls-remote", "--symref", "origin", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }

        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }

        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    status.success().then_some(())?;
    let branch = out.lines().find_map(|l| {
        let (branch, name) = l.strip_prefix("ref: refs/heads/")?.split_once('\t')?;
        (name == "HEAD" && !branch.is_empty()).then(|| branch.to_string())
    })?;
    let sha = out.lines().find_map(|l| {
        let (sha, name) = l.split_once('\t')?;
        (name == "HEAD" && sha.len() >= 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| sha.to_string())
    })?;
    Some((branch, sha))
}

pub fn command_base(command: &str, repo: &Path) -> String {
    command_base_in(&DaemonConfig::path(), command, repo)
}

fn command_base_in(file: &Path, command: &str, repo: &Path) -> String {
    if let Some(entry) = entry_in(file, repo) {
        return entry.base;
    }

    if matches!(command, "on-commit" | "on-push") {
        if !main_is_default(repo) {
            eprintln!(
                "shipcheck: no daemon entry for this checkout, so the hook reads shipcheck.toml from origin/main. Run `shipcheck install` to use the default branch"
            );
        }

        return origin_main();
    }

    remote_base(repo).unwrap_or_else(|| {
        eprintln!(
            "warning: origin did not say its default branch, so shipcheck reads shipcheck.toml from origin/main"
        );
        origin_main()
    })
}

/// The base from this checkout's daemon entry or the remote, None when neither gives one
pub fn known_base(repo: &Path) -> Option<String> {
    entry_in(&DaemonConfig::path(), repo)
        .map(|entry| entry.base)
        .or_else(|| remote_base(repo))
}

/// Whether the daemon watches this checkout, so it sees a PR head that was pushed from elsewhere
pub fn registered(repo: &Path) -> bool {
    entry_in(&DaemonConfig::path(), repo).is_some()
}

/// The daemon's entry for this repository, when it names this checkout and not another clone
fn entry_in(file: &Path, repo: &Path) -> Option<RepoEntry> {
    let checkout = |p: &Path| git::main_checkout(p).canonicalize().ok();
    let here = checkout(repo)?;
    git::repo_id(repo)
        .ok()
        .and_then(|id| DaemonConfig::load_from(file).ok()?.repos.remove(&id))
        .filter(|entry| checkout(&entry.path).as_ref() == Some(&here))
}

/// Whether the local refs agree main is the default branch, without asking the remote
fn main_is_default(repo: &Path) -> bool {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let head = git(&["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]);
    head.is_none_or(|h| h == "origin/main")
        && git(&["rev-parse", "--verify", "-q", "refs/remotes/origin/main"]).is_some()
}

pub fn posts_from(base: &str, repo: &Path) -> bool {
    posts_from_in(&DaemonConfig::path(), base, repo)
}

fn posts_from_in(file: &Path, base: &str, repo: &Path) -> bool {
    entry_in(file, repo).is_some_and(|entry| entry.base == base)
        || remote_base(repo).as_deref() == Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hooks_take_the_registered_base_and_never_ask_the_remote() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, origin) = (dir.path().join("app"), dir.path().join("acme/app.git"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&origin).unwrap();
        let git = |at: &Path, args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(at)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&origin, &["init", "-q", "--bare", "-b", "master"]);
        git(&repo, &["init", "-q", "-b", "master"]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "a",
            ],
        );
        git(&repo, &["push", "-q", "origin", "master"]);
        let file = dir.path().join("daemon.toml");

        // The remote says master, so only a call to it gives master here
        assert_eq!(command_base_in(&file, "run", &repo), "origin/master");

        for hook in ["on-commit", "on-push"] {
            assert_eq!(command_base_in(&file, hook, &repo), "origin/main");
        }

        // Only the remote's default branch posts until the daemon names a base
        assert!(posts_from_in(&file, "origin/master", &repo));
        assert!(!posts_from_in(&file, "origin/main", &repo));
        assert!(!posts_from_in(&file, "origin/dev", &repo));

        DaemonConfig::add_to(&file, "acme/app", &repo, "origin/dev").unwrap();

        // This checkout's entry comes first for every command
        for command in ["on-commit", "on-push", "run", "post"] {
            assert_eq!(command_base_in(&file, command, &repo), "origin/dev");
        }

        assert!(posts_from_in(&file, "origin/dev", &repo));
        assert!(posts_from_in(&file, "origin/master", &repo));
        assert!(!posts_from_in(&file, "origin/evil", &repo));
        assert!(!posts_from_in(&file, "evil", &repo));

        // Another clone of the same repository does not get the entry's base
        let other = dir.path().join("other");
        git(
            dir.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        assert!(!posts_from_in(&file, "origin/dev", &other));
        assert!(posts_from_in(&file, "origin/master", &other));

        // Offline, nothing posts, not even the fallback origin/main
        git(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                dir.path().join("gone").to_str().unwrap(),
            ],
        );
        DaemonConfig::add_to(&file, "acme/app", &other, "origin/dev").unwrap();
        assert!(!posts_from_in(&file, "origin/master", &repo));
        assert!(!posts_from_in(&file, "origin/main", &repo));

        // With no entry for this checkout and no answer, a command falls back to origin/main
        assert_eq!(command_base_in(&file, "run", &repo), "origin/main");
        assert_eq!(command_base_in(&file, "on-push", &repo), "origin/main");
        assert!(!main_is_default(&repo));
    }
}
