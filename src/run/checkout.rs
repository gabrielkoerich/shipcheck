use super::{CONFIG, Ctx, WRONG_TREE, prune_worktrees};
use crate::config::RunnerKind;
use crate::git::git;
use crate::workflow::WORKFLOWS;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

pub(super) fn checkout(
    ctx: &Ctx<'_>,
    kind: RunnerKind,
    template: &Path,
    dir: &Path,
    say: &dyn Fn(String),
) -> Result<(), String> {
    if dir.exists() {
        empty_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        prune_worktrees(&ctx.plan.repo);
    }

    let parent = template.parent().ok_or("template has no parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    // Host checks build as the developer does, so git-cow warms their template from the
    // developer's checkout, with the repository's `cow.share` and `cow.warmInterval`
    let opts = if kind == RunnerKind::Host {
        git_cow::Options::from_repo(&ctx.plan.repo)
    } else {
        // Host builds in the developer's checkout break in a Linux container, so git-cow never warms from it
        git_cow::Options {
            warm_interval: Duration::MAX,
            ..Default::default()
        }
    };

    let added = git_cow::add_or_plain(
        &ctx.plan.repo,
        template,
        &ctx.plan.base,
        dir,
        None,
        Some(&ctx.plan.sha),
        &opts,
    )
    .map_err(|e| e.to_string())?;

    if let git_cow::Added::Plain(e) = added {
        say(format!(
            "copy-on-write clone failed ({e}), using a plain worktree"
        ));
    }

    touch_tracked(dir)?;
    verify_checkout(dir, &ctx.plan.tree)
}

pub(super) fn touch_tracked(dir: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git ls-files: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "git ls-files: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let now = SystemTime::now();

    for name in out.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let path = dir.join(std::ffi::OsStr::from_bytes(name));

        // A symlink or a submodule's directory keeps its time, only regular files are built from
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            continue;
        }

        fs::File::open(&path)
            .and_then(|f| f.set_modified(now))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }

    Ok(())
}

pub(super) fn verify_checkout(dir: &Path, tree: &str) -> Result<(), String> {
    let index = dir.with_extension("verify-index");
    let _ = fs::remove_file(&index);
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .map_err(|e| format!("git: {e}"))?;

        if !out.status.success() {
            return Err(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let on_disk = git(&["read-tree", "HEAD"])
        .and_then(|_| git(&["add", "-u"]))
        .and_then(|_| git(&["write-tree"]));
    let _ = fs::remove_file(&index);
    let on_disk = on_disk?;

    if on_disk != tree {
        return Err(format!(
            "{WRONG_TREE}: shipcheck's clone {} is tree {on_disk}, not {tree}. Run again, and report it at https://github.com/gabrielkoerich/shipcheck/issues if it repeats",
            dir.display()
        ));
    }

    Ok(())
}

pub(super) fn empty_dir(dir: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();

        if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }

    Ok(())
}

pub(super) fn standalone_git(dir: &Path, sha: &str) -> Result<Option<String>, String> {
    let link = dir.join(".git");
    let text = fs::read_to_string(&link).unwrap_or_default();
    let Some(gitdir) = text.trim().strip_prefix("gitdir: ").map(PathBuf::from) else {
        // A plain worktree fallback or a clone already has its own `.git` directory
        return Ok(None);
    };
    let copy = dir.join(".git.shipcheck");
    let _ = fs::remove_dir_all(&copy);
    let copy_s = copy.to_string_lossy().into_owned();
    git(Path::new("/"), &["init", "-q", "--bare", &copy_s])?;
    // Only tags in the tested commit's history, since a tag on a local commit would carry it along
    let tags = job_git(
        dir,
        &gitdir,
        &[
            "for-each-ref",
            "--merged",
            "HEAD",
            "--format=+%(refname):%(refname)",
            "refs/tags",
        ],
    )?;
    // From the worktree's own git dir, never through the checkout's `.git`
    let source = gitdir.to_string_lossy().into_owned();
    // A shallow source rejects HEAD without --update-shallow, and the job then has no parents to read
    let mut fetch = vec![
        "fetch",
        "-q",
        "--update-shallow",
        "--no-tags",
        "--no-write-fetch-head",
        &source,
        "HEAD",
    ];
    fetch.extend(tags.lines());
    git(&copy, &fetch)?;
    let _ = fs::remove_dir_all(copy.join("hooks"));
    fs::write(
        copy.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
    )
    .map_err(|e| e.to_string())?;
    // A branch, so act finds a ref for `github.ref`, never `main`, so deploy guards on main still skip
    fs::create_dir_all(copy.join("refs/heads")).map_err(|e| e.to_string())?;
    fs::write(copy.join("refs/heads/shipcheck"), format!("{sha}\n")).map_err(|e| e.to_string())?;
    fs::write(copy.join("HEAD"), "ref: refs/heads/shipcheck\n").map_err(|e| e.to_string())?;
    fs::copy(gitdir.join("index"), copy.join("index")).map_err(|e| e.to_string())?;
    fs::remove_file(&link).map_err(|e| e.to_string())?;
    fs::rename(&copy, &link).map_err(|e| e.to_string())?;
    Ok(Some(text))
}

/// git on a job's checkout through the worktree's git dir on the host, so it never reads a `.git`
/// in the checkout, and with no fsmonitor or hooks whatever a config says
pub(super) fn job_git(dir: &Path, gitdir: &Path, args: &[&str]) -> Result<String, String> {
    let (gitdir, dir) = (gitdir.to_string_lossy(), dir.to_string_lossy());
    let mut all = vec![
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "--git-dir",
        &gitdir,
        "--work-tree",
        &dir,
    ];
    all.extend(args);
    git(Path::new("/"), &all)
}

/// A commit can make the paths shipcheck writes into symlinks to host paths, which writing through
/// them would overwrite. The links go, their targets stay
pub(super) fn remove_planted_links(dir: &Path) -> Result<(), String> {
    for path in [dir.join(".github"), dir.join(WORKFLOWS), dir.join(CONFIG)] {
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }

    Ok(())
}
