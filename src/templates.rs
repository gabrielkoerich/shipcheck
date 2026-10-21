use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::run::git_lock_in;

/// A template no run used for this long goes, the next run of its job clones it again
pub const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug)]
pub struct Template {
    /// `owner/repo`
    pub repo: String,
    pub path: PathBuf,
    pub used: SystemTime,
}

/// Every template under `root`: `<owner>/<repo>/<file>/<job>`, or `<owner>/<repo>` as older
/// versions made it, with when a run last used it
pub fn list(root: &Path) -> Vec<Template> {
    let mut found = Vec::new();
    walk(root, root, 0, &mut found);
    found
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Template>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');

        if hidden || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }

        if depth >= 1 && path.join(".git").exists() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let repo: Vec<_> = rel.iter().take(2).map(|p| p.to_string_lossy()).collect();
            out.push(Template {
                repo: repo.join("/"),
                used: last_used(&path),
                path,
            });
        } else if depth < 3 {
            walk(root, &path, depth + 1, out);
        }
    }
}

/// git-cow's lock and stamp beside a template, `.<job>.lock`, `.<job>.warmed` and `.<job>.warm`
fn beside(template: &Path, ext: &str) -> PathBuf {
    let name = template.file_name().unwrap_or_default().to_string_lossy();
    template.with_file_name(format!(".{name}.{ext}"))
}

/// The newest of the template's own time and git-cow's lock and stamp, which every clone touches
fn last_used(template: &Path) -> SystemTime {
    [
        template.to_path_buf(),
        beside(template, "lock"),
        beside(template, "warmed"),
    ]
    .iter()
    .filter_map(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
    .max()
    .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// The templates no run used within [`KEEP`] of `now`
pub fn stale(root: &Path, now: SystemTime) -> Vec<Template> {
    list(root)
        .into_iter()
        .filter(|t| now.duration_since(t.used).is_ok_and(|age| age > KEEP))
        .collect()
}

/// Whether a lock file other than `own` under `dir` is held, by a run of this repository
fn busy(dir: &Path, own: &Path) -> bool {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "lock") && p != own)
        .any(|p| fs::File::open(&p).is_ok_and(|f| f.try_lock().is_err()))
}

/**
 * Removes the stale templates of every repository with no other run going, with git-cow, then
 * prunes the repository's worktree list. A template whose lock is held is being cloned and stays.
 * `own` is the slot lock the calling run holds, which a second open of it would see as busy
 **/
pub fn prune(home: &Path, now: SystemTime, own: &Path) {
    let root = home.join("templates");
    let mut by_repo: BTreeMap<String, Vec<Template>> = BTreeMap::new();

    for t in stale(&root, now) {
        by_repo.entry(t.repo.clone()).or_default().push(t);
    }

    for (repo, templates) in by_repo {
        if busy(&home.join("runs").join(&repo), own) {
            continue;
        }

        let Ok(_git) = git_lock_in(home, &repo) else {
            continue;
        };
        let mut gitdirs = Vec::new();

        for t in templates {
            let held = fs::File::open(beside(&t.path, "lock")).is_ok_and(|f| f.try_lock().is_err());

            if held {
                continue;
            }

            let gitdir = common_dir(&t.path);
            let removed = gitdir
                .as_deref()
                .is_some_and(|g| git_cow::remove_template(g, &t.path).is_ok());

            // The repository is gone or refused, so nothing of it needs the files
            if !removed {
                let _ = fs::remove_dir_all(&t.path);

                for ext in ["lock", "warmed"] {
                    let _ = fs::remove_file(beside(&t.path, ext));
                }

                let _ = fs::remove_dir_all(beside(&t.path, "warm"));
            }

            gitdirs.extend(gitdir);
            remove_empty_parents(&root, &t.path);
        }

        gitdirs.sort();
        gitdirs.dedup();

        for dir in gitdirs {
            let _ = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["worktree", "prune"])
                .status();
        }
    }
}

/// The repository's `.git` from the template's `.git` file, `gitdir: <repo>/.git/worktrees/<name>`
fn common_dir(template: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(template.join(".git")).ok()?;
    let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir: ")?);
    let common = gitdir.parent()?.parent()?.to_path_buf();
    common.exists().then_some(common)
}

fn remove_empty_parents(root: &Path, path: &Path) {
    let mut dir = path.parent();

    while let Some(d) = dir.filter(|d| d.starts_with(root) && *d != root) {
        // Fails on a directory that still holds something, which ends the climb
        if fs::remove_dir(d).is_err() {
            break;
        }

        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(root: &Path, rel: &str, age_days: u64) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(".git"), "gitdir: /nonexistent/.git/worktrees/x").unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_days * 24 * 3600);
        fs::File::open(&path).unwrap().set_modified(when).unwrap();
        path
    }

    #[test]
    fn stale_means_no_use_for_seven_days() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        template(root, "acme/app/ci.yml/test", 10);
        let fresh = template(root, "acme/app/ci.yml/lint", 10);
        // A clone touches git-cow's lock beside the template, which makes it recent
        fs::write(beside(&fresh, "lock"), "").unwrap();
        template(root, "acme/old", 30);
        template(root, "acme/web/host/build", 2);

        let mut names: Vec<_> = stale(root, SystemTime::now())
            .into_iter()
            .map(|t| (t.repo, t.path.strip_prefix(root).unwrap().to_path_buf()))
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                (
                    "acme/app".to_string(),
                    PathBuf::from("acme/app/ci.yml/test")
                ),
                ("acme/old".to_string(), PathBuf::from("acme/old")),
            ]
        );
    }

    #[test]
    fn prune_skips_a_repository_with_a_run_going() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let root = home.join("templates");
        let gone = template(&root, "acme/app/ci.yml/test", 10);
        let kept = template(&root, "acme/busy/ci.yml/test", 10);
        fs::create_dir_all(home.join("runs/acme/busy")).unwrap();
        let other = fs::File::create(home.join("runs/acme/busy/0.lock")).unwrap();
        other.lock().unwrap();
        let own = home.join("runs/acme/app/0.lock");

        prune(home, SystemTime::now(), &own);
        assert!(!gone.exists());
        assert!(!root.join("acme/app").exists(), "empty parents go too");
        assert!(kept.exists());
    }
}
