use crate::config;
use crate::git::{git, repo_id};
use crate::paths::home;
use crate::runner::act::Act;
use crate::runner::host::Host;
use crate::runner::{Runner, host};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Deletes results untouched for 14 days and logs untouched for 90, so a busy machine does not fill
/// its disk. Logs stay longer for reading an old failure, and they are small next to the templates
pub fn prune(home: &Path, now: SystemTime) {
    for (dir, keep) in [("cache", KEEP_RESULTS), ("logs", KEEP_LOGS)] {
        let cutoff = now - keep;
        let Ok(entries) = fs::read_dir(home.join(dir)) else {
            continue;
        };

        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t < cutoff);

            if !old {
                continue;
            }

            let path = entry.path();
            let _ = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
        }
    }
}

pub(super) const KEEP_RESULTS: Duration = Duration::from_hours(336);

pub(super) const KEEP_LOGS: Duration = Duration::from_hours(2160);

/// What `clean` removes: one repository's state, or every repository's
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    Repo(&'a Path),
    All,
}

pub fn clean(scope: Scope<'_>) -> Result<Vec<String>, String> {
    let home = home();
    let mut done = Vec::new();
    let (repo, id) = match scope {
        Scope::Repo(path) => (Some(path), Some(repo_id(path)?)),
        Scope::All => (None, None),
    };
    let runs = match &id {
        Some(id) => home.join("runs").join(id),
        None => home.join("runs"),
    };
    // A branch run also holds the lock of its host cache's `runs/run-X`
    let locks = glob_locks(&runs)
        .unwrap_or_default()
        .into_iter()
        .chain(host::run_locks(id.as_deref()));

    for lock in locks {
        // A lock that cannot be opened is no run's
        let Ok(file) = fs::File::open(&lock) else {
            continue;
        };

        if file.try_lock().is_err() {
            return Err("a run is in progress, clean once it finishes".into());
        }
    }

    done.extend(Act::default().clean(id.as_deref()));
    done.extend(Host::new(config::Host::default(), Path::new(".")).clean(id.as_deref()));
    let mut dirs = vec![runs];

    match &id {
        Some(id) => {
            let template = home.join("templates").join(id);

            // One template per job under `<file>/<job>`, and the single one older runs kept here
            if let Some(path) = repo {
                let jobs = fs::read_dir(&template)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .flat_map(|file| fs::read_dir(file.path()).into_iter().flatten().flatten())
                    .map(|job| job.path())
                    .filter(|job| job.is_dir());

                for job in jobs.collect::<Vec<_>>() {
                    let _ = git_cow::remove_template(path, &job);
                }

                let _ = git_cow::remove_template(path, &template);
            }

            dirs.push(template);
        }
        None => dirs.extend(
            [
                "templates",
                "cache",
                "logs",
                "background",
                "locks",
                "records",
            ]
            .map(|d| home.join(d)),
        ),
    }

    for dir in dirs.into_iter().filter(|d| d.exists()) {
        fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        done.push(format!("removed {}", dir.display()));
    }

    // The template was a worktree of the repository, so git still lists it until pruned
    if let Some(path) = repo {
        let _ = git(path, &["worktree", "prune"]);
    }

    Ok(done)
}

/// Every regular `<n>.lock` file under `dir`, at any depth, never through a symlink
pub(super) fn glob_locks(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();

    for entry in fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };

        if kind.is_dir() {
            found.extend(glob_locks(&path).unwrap_or_default());
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "lock") {
            found.push(path);
        }
    }

    Ok(found)
}
