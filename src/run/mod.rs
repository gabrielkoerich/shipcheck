use crate::base::remote_base_sha;
use crate::clock::timing;
use crate::clock::unix_now;
use crate::config::Check;
use crate::git::repo_id;
use crate::git::{git, git_raw};
use crate::history;
use crate::paths::git_lock_in;
use crate::paths::home;
use crate::paths::log_dir;
use crate::paths::partial_log_dir;
use crate::paths::secrets_file;
use crate::runner::act;
use crate::runner::act::Act;
use crate::runner::host;
use crate::runner::host::Host;
use crate::secret;
use crate::templates;
use crate::trust;
use crate::ui::printable;
use crate::workflow::{WORKFLOWS, false_on_every_pr};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::{self, KeyInputs};
use crate::clock::clock;
use crate::config::{self, RepoConfig, RunnerKind};
use crate::queue;
use crate::runner::host::{self as host_runner, HOST, runner_of};
use crate::runner::shard::{self, Rejected, ShardRun};
use crate::runner::{RunRequest, Runner, Runners};
use crate::workflow;

const CONFIG: &str = "shipcheck.toml";

/// Job ids per file a run reports: the `[host]` checks under [`HOST`], or each required workflow's jobs
pub type Jobs = BTreeMap<String, BTreeSet<String>>;

/// Set in init's preview process, whose trials then leave the repository's worktree entries alone
pub const PREVIEW: &str = "SHIPCHECK_INIT_PREVIEW";

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub sha: String,
    pub tree: String,
    pub base: String,
    pub passed: bool,
    pub workflows: Vec<WorkflowResult>,
    pub finished_unix: u64,
    /// Why nothing ran, when the change touches no file under `paths`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// What the commit changes in its workflows and `shipcheck.toml`, which ran from the base
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ci_changes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowResult {
    pub file: String,
    pub exit: Option<i32>,
    pub attempts: u32,
    pub log_sha256: String,
    pub jobs: BTreeMap<String, String>,
    pub passed: bool,
    /// The jobs that had a sharded pytest step
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub shards: BTreeMap<String, ShardRun>,
    /// When each job that ran started and ended, in Unix seconds. Entries from before have none
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub times: BTreeMap<String, [u64; 2]>,
    /// The jobs kept from the branch's last result, by the tree they passed on
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub kept: BTreeMap<String, String>,
    /// What each job wrote to `$GITHUB_STEP_SUMMARY`, for the PR comment. Entries from before have none
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub summaries: BTreeMap<String, String>,
}

/// The state of a job shipcheck could not start, such as when Docker failed to create its network
pub const COULD_NOT_RUN: &str = "failure, shipcheck could not run it";

/// The state of a job whose checkout was not the tested tree, a failed proof and so the check's failure
pub const WRONG_TREE: &str = "failure, the checkout is not the tested tree";

/// The state of a job that ended in error `e`: a wrong tree fails the code's check, anything else
/// means shipcheck could not run it
fn error_state(e: &str) -> &'static str {
    if e.starts_with(WRONG_TREE) {
        WRONG_TREE
    } else {
        COULD_NOT_RUN
    }
}

impl Entry {
    /// A job's clone was not the tested tree, a fault of shipcheck's own that another run may not repeat
    fn wrong_tree(&self) -> bool {
        self.workflows
            .iter()
            .flat_map(|w| w.jobs.values())
            .any(|s| s.starts_with(WRONG_TREE))
    }

    /// It failed only because shipcheck could not start jobs, so the code never ran
    pub fn could_not_run(&self) -> bool {
        let states = || self.workflows.iter().flat_map(|w| w.jobs.values());
        let fine = |s: &str| s == "success" || s == "skipped" || s.ends_with(ALLOWED);

        !self.passed
            && states().any(|s| s == COULD_NOT_RUN)
            && states().all(|s| fine(s) || s == COULD_NOT_RUN || s == NOT_RUN)
    }
}

/// The exit code of a finished run: 0 passed, 1 the code failed, 2 shipcheck could not run it
pub fn exit_code(entry: &Entry) -> u8 {
    if entry.passed {
        0
    } else if entry.could_not_run() {
        2
    } else {
        1
    }
}

/// A job whose need did not pass
const NOT_RUN: &str = "not run, a job it needs did not pass";

/// Everything about one commit that the cache key and a run need, read from git
#[derive(Debug)]
pub struct Plan {
    pub repo: PathBuf,
    pub sha: String,
    pub tree: String,
    pub base: String,
    pub config_text: Vec<u8>,
    pub config: RepoConfig,
    pub workflows: BTreeMap<String, Vec<u8>>,
    pub key: String,
    /// What the commit changes in CI, which did not run
    pub ci_changes: Vec<String>,
    /// The runners' identity in the key: act's version, then the host checks' tool versions
    pub tools: String,
    /// The branch the run is for, from the hooks and the daemon or the checkout, none when detached
    pub branch: Option<String>,
    /// The key with no tree: two runs with the same one differ only in the code they test
    pub inputs: String,
}

/// Clones, run slots and tool caches, which every run can rebuild and which hold hundreds of GB
#[cfg(target_os = "macos")]
const REBUILT: [&str; 3] = ["templates", "runs", "hostcache"];

/**
 * Drops the repository's worktree entries whose folders are gone. Not in init's preview, whose
 * trials remove their own and must leave the repository's other entries as they are
 **/
fn prune_worktrees(repo: &Path) {
    if std::env::var_os(PREVIEW).is_none() {
        let _ = git(repo, &["worktree", "prune"]);
    }
}

/**
 * Keeps Time Machine off what a run can rebuild, so a backup holds the results and logs and not
 * clones of every repository. `tmutil addexclusion` sets a flag on the folder itself, needs no root
 * and follows the folder, so it runs only when the flag is missing
 **/
#[cfg(not(target_os = "macos"))]
fn exclude_from_backup(_: &Path) {}

#[cfg(target_os = "macos")]
fn exclude_from_backup(home: &Path) {
    // macOS keeps its temporary folder out of backups already, and tmutil takes seconds a folder
    let temp = std::env::temp_dir();

    if home.starts_with(&temp) || temp.canonicalize().is_ok_and(|t| home.starts_with(t)) {
        return;
    }

    for dir in REBUILT.map(|d| home.join(d)) {
        if fs::create_dir_all(&dir).is_err() || excluded(&dir) {
            continue;
        }

        let _ = Command::new("tmutil")
            .arg("addexclusion")
            .arg(&dir)
            .output();
    }
}

#[cfg(target_os = "macos")]
fn excluded(dir: &Path) -> bool {
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a size query with a null buffer reads nothing
    unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"com.apple.metadata:com_apple_backup_excludeItem".as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        ) > 0
    }
}

/// Reads the commit, and the config and workflows from `base`, never from the commit or the working copy
#[expect(
    clippy::too_many_lines,
    reason = "the plan's inputs, in the order the cache key reads them"
)]
pub fn plan(cwd: &Path, rev: &str, base_ref: &str) -> Result<Plan, String> {
    let repo = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);
    let sha = git(
        &repo,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )?;
    let tree = git(&repo, &["rev-parse", &format!("{sha}^{{tree}}")])?;
    let base = git(
        &repo,
        &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
    )?;

    let mut workflows = BTreeMap::new();

    for path in git(
        &repo,
        &["ls-tree", "--name-only", &base, &format!("{WORKFLOWS}/")],
    )?
    .lines()
    {
        if std::path::Path::new(path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("yml"))
            || std::path::Path::new(path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml"))
        {
            workflows.insert(
                path.to_string(),
                git_raw(&repo, &["show", &format!("{base}:{path}")])?,
            );
        }
    }

    // Only the owner's file says what runs here, a job init kept on GitHub may never finish locally
    let config_text = git_raw(&repo, &["show", &format!("{base}:{CONFIG}")]).map_err(|_| {
        format!(
            "{base_ref} has no {CONFIG}, so there is nothing to run. Run `shipcheck init` (`--yes` from a script or an agent) and merge {CONFIG} to main"
        )
    })?;
    let mut config = RepoConfig::parse(&String::from_utf8_lossy(&config_text))?;
    let files: BTreeSet<String> = workflows.keys().map(|p| file_name(p)).collect();
    config.validate(&files)?;

    // Where each job runs needs the workflows, so a wrong entry fails here, before anything runs
    if !config.jobs.is_empty() {
        expected_jobs(&config, |file| {
            workflows
                .iter()
                .find(|(path, _)| file_name(path) == file)
                .map(|(_, body)| String::from_utf8_lossy(body).into_owned())
        })?;
    }

    let tree_files = git_raw(&repo, &["ls-tree", "-r", "-z", "--name-only", &tree])?;
    config.act.for_tree(
        String::from_utf8_lossy(&tree_files)
            .split('\0')
            .filter(|f| !f.is_empty()),
    );

    // Only this machine can let a check out of the sandbox, never the repository alone
    if config.host.checks.values().any(|c| !c.sandbox) && !trust::trusted(&repo) {
        for c in config.host.checks.values_mut().filter(|c| !c.sandbox) {
            c.sandbox = true;
            c.untrusted = true;
        }
    }

    // Hash required workflows as they run, so a change to the injection changes the key too
    let runners = Runners::new(&config, &repo);
    let act_files = config.act_files();
    let mut as_run = BTreeMap::new();

    // Host checks run no workflow, so with no act job a change to one does not change the result
    for (path, body) in workflows.iter().filter(|_| config.acts()) {
        let body = if act_files.contains(&file_name(path)) {
            as_run_body(
                runners.for_file(&file_name(path))?,
                body,
                &file_name(path),
                "nonce",
            )?
        } else {
            body.clone()
        };
        as_run.insert(path.clone(), body);
    }

    let mut tools = runners.identity()?;

    // So trusting a repository runs its unsandboxed checks again, rather than reuse a sandboxed result
    for (name, _) in config.host.checks.iter().filter(|(_, c)| !c.sandbox) {
        let _ = write!(tools, "\nunsandboxed\0{name}");
    }

    let platforms = act::platforms(&config.act);
    let inputs = |tree: &str| {
        cache::key(&KeyInputs {
            tree,
            config: &config_text,
            workflows: &as_run,
            runner_kind: runners.kind(),
            checks: &config.host.checks,
            runner_version: &tools,
            shipcheck_version: env!("CARGO_PKG_VERSION"),
            platforms: &platforms,
        })
    };
    let (key, inputs) = (inputs(&tree), inputs(""));
    let branch = std::env::var("SHIPCHECK_BRANCH")
        .ok()
        .or_else(|| git(&repo, &["symbolic-ref", "--short", "-q", "HEAD"]).ok())
        .filter(|b| !b.is_empty());

    let ci_changes = ci_changes(&repo, &base, &sha)?;

    Ok(Plan {
        repo,
        sha,
        tree,
        base,
        config_text,
        config,
        workflows,
        key,
        ci_changes,
        tools,
        branch,
        inputs,
    })
}

/// Which tree a snapshot takes: the working copy with every edit, or the index as it is
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tree {
    Working,
    Staged,
}

/**
 * The working copy, or with `Staged` the index, as a commit no ref names, with HEAD as parent, like
 * `git stash create`. The working copy is HEAD's tree with every edit and every file that is not
 * gitignored. It is built in a copy of the index, so the user's index, branches and stash stay as
 * they are. None when the tree is HEAD's
 **/
pub fn snapshot(cwd: &Path, tree: Tree) -> Result<Option<String>, String> {
    let repo = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);
    let head = git(&repo, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let (index, temp) = index_paths(&repo)?;
    let what = match tree {
        Tree::Staged => "index",
        Tree::Working => "working copy",
    };
    let tree = snapshot_tree(&repo, &index, &temp, tree);
    let _ = fs::remove_file(&temp);
    let tree = tree?;

    if tree == git(&repo, &["rev-parse", "HEAD^{tree}"])? {
        return Ok(None);
    }

    let sha = git(
        &repo,
        &[
            "-c",
            "user.name=shipcheck",
            "-c",
            "user.email=shipcheck@localhost",
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-m",
            &format!("shipcheck: the {what}"),
        ],
    )?;
    Ok(Some(sha))
}

/// The worktree's real index, and the path of this process's copy of it
fn index_paths(repo: &Path) -> Result<(PathBuf, PathBuf), String> {
    let index = PathBuf::from(git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?);
    let temp = index.with_file_name(format!("shipcheck-index-{}", std::process::id()));
    Ok((index, temp))
}

/**
 * git rehashes an entry whose file changed no earlier than the index was written, since its stats
 * cannot tell an edit in that second apart. A copy with a new time loses that, and an edit of the
 * same size in the same second as `git add` went unseen. Linux's copy takes the time of the copy
 **/
fn copy_index(index: &Path, temp: &Path) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", index.display());
    fs::copy(index, temp).map_err(err)?;
    let written = fs::metadata(index)
        .and_then(|m| m.modified())
        .map_err(err)?;
    fs::File::options()
        .write(true)
        .open(temp)
        .and_then(|f| f.set_modified(written))
        .map_err(|e| format!("{}: {e}", temp.display()))
}

fn snapshot_tree(repo: &Path, index: &Path, temp: &Path, tree: Tree) -> Result<String, String> {
    let with_temp = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_INDEX_FILE", temp)
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

    // A copy of the real index keeps its file stats, so git hashes only the files that changed
    if index.exists() {
        copy_index(index, temp)?;
    } else {
        with_temp(&["read-tree", "HEAD"])?;
    }

    if tree == Tree::Working {
        with_temp(&["add", "-A"])?;
    }

    with_temp(&["write-tree"])
}

/// The untracked files that are not gitignored, which a run of the working copy includes
pub fn untracked(repo: &Path) -> Result<Vec<String>, String> {
    let out = git_raw(
        repo,
        &[
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .collect())
}

/// What the last `shipcheck run` of a worktree tested, which `shipcheck commit` commits
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tested {
    pub tree: String,
    pub staged: bool,
    pub passed: bool,
    /// A `--job` or `--workflow` run, which is never the check
    #[serde(default)]
    pub partial: bool,
}

impl Tested {
    /// Which tree the run took
    pub fn tree_kind(&self) -> Tree {
        if self.staged {
            Tree::Staged
        } else {
            Tree::Working
        }
    }
}

fn tested_file(repo: &Path) -> PathBuf {
    let id = cache::hex(&Sha256::digest(repo.to_string_lossy().as_bytes()));
    home().join("tested").join(format!("{}.json", &id[..16]))
}

pub fn record_tested(repo: &Path, tested: &Tested) -> Result<(), String> {
    let file = tested_file(repo);
    fs::create_dir_all(file.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
    let text = serde_json::to_string(tested).map_err(|e| e.to_string())?;
    fs::write(&file, text).map_err(|e| e.to_string())
}

/**
 * The remote's default branch as `origin/<branch>`, None when the remote did not answer. Asked once
 * per process for each checkout and origin URL. Never the local origin/HEAD, which anyone with the
 * clone can repoint at a branch whose shipcheck.toml and workflows would then run
 **/
/**
 * The default branch, `base` without `origin/`, when `commit` (no `sha`) runs with HEAD on it, or
 * when `post` names its tip, locally or on `base`. Both refuse there, PRs carry the checks
 **/
pub fn on_default_branch(repo: &Path, base: &str, sha: Option<&str>) -> Option<String> {
    let name = base.strip_prefix("origin/").unwrap_or(base);
    let tip = |r: &str| {
        git(
            repo,
            &["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")],
        )
    };
    let on = match sha {
        None => git(repo, &["symbolic-ref", "--short", "-q", "HEAD"]).is_ok_and(|b| b == name),
        Some(sha) => [format!("refs/heads/{name}"), base.to_string()]
            .iter()
            .any(|r| tip(r).is_ok_and(|t| t == sha)),
    };

    on.then(|| name.to_string())
}

pub fn last_tested(repo: &Path) -> Option<Tested> {
    serde_json::from_str(&fs::read_to_string(tested_file(repo)).ok()?).ok()
}

/// The caller's `PATH`, saved before `main` keeps only the trusted entries for shipcheck's own processes
pub static CALLER_PATH: std::sync::OnceLock<std::ffi::OsString> = std::sync::OnceLock::new();

/**
 * Commits exactly the tree the last run tested, through `git commit`, so hooks, signing and the
 * author work as usual. The commit uses a copy of the index built by the snapshot's rules, and
 * the tree is compared on that copy, so what is compared is what is committed. The copy becomes
 * the real index only after the commit. On any refusal the real index is untouched
 **/
/// Whether `commit` takes a tree whose run failed, as `--allow-failed` asks
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failed {
    Refuse,
    Allow,
}

pub fn commit(
    cwd: &Path,
    tested: &Tested,
    git_args: &[String],
    failed: Failed,
) -> Result<String, String> {
    let repo = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);

    if tested.partial {
        return Err(
            "the last run was partial, with --job or --workflow, which is not the check: run `shipcheck run`, then `shipcheck commit`".into(),
        );
    }

    if !tested.passed && failed == Failed::Refuse {
        return Err(
            "the run failed, fix it and run `shipcheck run` again, or pass --allow-failed".into(),
        );
    }

    let (index, temp) = index_paths(&repo)?;
    let committed = snapshot_tree(&repo, &index, &temp, tested.tree_kind()).and_then(|tree| {
        if tree != tested.tree {
            return Err("files changed since the run, run `shipcheck run` again".to_string());
        }

        // git comes from the trusted PATH, but its hooks are the user's own and get the caller's PATH,
        // such as ~/.nvm's node for husky, which the trusted one drops for sitting under a .git
        let trusted = std::env::var_os("PATH").unwrap_or_default();
        let bin = std::env::split_paths(&trusted)
            .map(|d| d.join("git"))
            .find(|g| g.is_file())
            .unwrap_or_else(|| "git".into());
        let mut git_commit = Command::new(bin);

        if let Some(path) = CALLER_PATH.get() {
            git_commit.env("PATH", path);
        }

        // From the caller's directory, so a relative `-F` file is found. Agents commit unattended, so
        // a signing prompt from a key agent would only stall them
        let status = git_commit
            .current_dir(cwd)
            .args(["-c", "commit.gpgsign=false", "commit"])
            .args(git_args)
            .env("GIT_INDEX_FILE", &temp)
            .status()
            .map_err(|e| format!("git: {e}"))?;

        if !status.success() {
            return Err("git commit failed".into());
        }

        fs::rename(&temp, &index).map_err(|e| format!("{}: {e}", index.display()))
    });

    if committed.is_err() {
        let _ = fs::remove_file(&temp);
    }

    committed?;
    let sha = git(&repo, &["rev-parse", "HEAD"])?;

    // A pre-commit hook can change what is committed, then that commit runs again
    if git(&repo, &["rev-parse", "HEAD^{tree}"])? != tested.tree {
        return Err(format!(
            "committed {}, but a hook changed its files, so it is not the tree that ran. Run `shipcheck run --sha HEAD`",
            &sha[..12]
        ));
    }

    Ok(sha)
}

/**
 * What the commit changes in its workflows and `shipcheck.toml` since it left the base. These never
 * run, but once merged they change CI for everyone, so the check names them, and the jobs
 * a change lets fail or never runs. Also the files that define what a command does, such as
 * package.json scripts, which do run from the commit in either mode
 **/
fn ci_changes(repo: &Path, base: &str, sha: &str) -> Result<Vec<String>, String> {
    let Ok(merge_base) = git(repo, &["merge-base", base, sha]) else {
        return Ok(vec![
            "no history in common with the base, so changes to CI are unknown".into(),
        ]);
    };
    let changed = git_raw(
        repo,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            &merge_base,
            sha,
            "--",
            WORKFLOWS,
            CONFIG,
        ],
    )?;
    let show = |rev: &str, path: &str| {
        git_raw(repo, &["show", &format!("{rev}:{path}")])
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    let mut found = Vec::new();

    for path in changed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(path);
        let too_big = [merge_base.as_str(), sha].iter().any(|rev| {
            git(repo, &["cat-file", "-s", &format!("{rev}:{path}")])
                .ok()
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|n| n > workflow::MAX_COMPARED)
        });

        if too_big {
            found.push(format!(
                "{} changes, too large to compare",
                workflow::shown(&path)
            ));
            continue;
        }

        match (show(&merge_base, &path), show(sha, &path)) {
            (Some(old), Some(new)) => {
                found.push(format!("{} changes", workflow::shown(&path)));

                if path.starts_with(WORKFLOWS) {
                    let weakened = workflow::weakened_jobs(&old, &new);
                    let file = workflow::shown(&path);
                    found.extend(weakened.into_iter().map(|w| format!("{file}: {w}")));
                }
            }
            (None, Some(_)) => found.push(format!("{} is added", workflow::shown(&path))),
            (Some(_), None) => found.push(format!("{} is removed", workflow::shown(&path))),
            (None, None) => {}
        }
    }

    // Under act too, since a workflow's `run: npm test` reads the branch's package.json the same way
    found.extend(check_definition_changes(repo, &merge_base, sha)?);

    Ok(found)
}

/// Files changed since the merge base whose part that defines what a command runs changed
fn check_definition_changes(
    repo: &Path,
    merge_base: &str,
    sha: &str,
) -> Result<Vec<String>, String> {
    let changed = git_raw(
        repo,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            merge_base,
            sha,
        ],
    )?;
    let def = |rev: &str, path: &str| {
        git_raw(repo, &["show", &format!("{rev}:{path}")])
            .ok()
            .and_then(|b| host_runner::check_definition(path, &String::from_utf8_lossy(&b)))
    };
    let mut found = Vec::new();

    for path in changed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(path);

        // Only names that can define a check are read, so a large data file is never loaded
        if host_runner::check_definition(&path, "").is_none() {
            continue;
        }

        let shown = workflow::shown(&path);

        match (def(merge_base, &path), def(sha, &path)) {
            (Some(old), Some(new)) if old != new => {
                found.push(format!("{shown} changes what checks may run"));
            }
            (None, Some(_)) => {
                found.push(format!("{shown} is added, it can change what checks run"));
            }
            (Some(_), None) => {
                found.push(format!("{shown} is removed, it can change what checks run"));
            }
            _ => {}
        }
    }

    Ok(found)
}

/// The jobs a run of `config` reports, read from each required workflow's source as `source` gives it
pub fn expected_jobs(
    config: &RepoConfig,
    source: impl Fn(&str) -> Option<String>,
) -> Result<Jobs, String> {
    let mut graphs = BTreeMap::new();

    for file in config.act_files() {
        let body = source(&file).ok_or_else(|| format!("{file} is not on the base"))?;
        graphs.insert(file, workflow::job_needs(&body)?);
    }

    let mut jobs = act_jobs(config, &graphs)?;

    // Both would post with a prefix, so a rule that requires shipcheck/<name> would wait forever
    for name in config.host.checks.keys() {
        if let Some((file, _)) = jobs.iter().find(|(_, ids)| ids.contains(name)) {
            return Err(format!(
                "host check `{name}` and job `{file}/{name}` would both post as shipcheck/{name}: rename one, for example the host check to `{name}-{}`",
                std::env::consts::OS
            ));
        }
    }

    if config.hosts() {
        jobs.insert(
            HOST.to_string(),
            config.host.checks.keys().cloned().collect(),
        );
    }

    Ok(jobs)
}

/**
 * The act jobs of `config` per workflow file: each listed workflow whole, each act `[jobs]` entry,
 * and every job they need, less the jobs host entries take out. A host entry must name a job of its
 * file and a host check of that id, so no job is dropped without a check in its place
 **/
fn act_jobs(config: &RepoConfig, graphs: &Graphs) -> Result<Jobs, String> {
    let mut chosen = select(graphs, &config.jobs_on(RunnerKind::Act), &config.workflows)?;
    let moved = config.jobs_on(RunnerKind::Host);

    // Every host entry first, so a moved job may need another moved job
    for name in &moved {
        let (file, job) = name.split_once('/').unwrap_or((name, ""));
        let real = graphs
            .get(file)
            .is_some_and(|graph| graph.iter().any(|(id, _)| names_job(id, job)));

        if !real {
            return Err(format!("jobs entry {name:?} names no job of {file}"));
        }

        if !config.host.checks.contains_key(job) {
            return Err(format!(
                "jobs entry {name:?} is on the host runner, but [host.checks] has no check {job:?} to run in its place"
            ));
        }

        if let Some(picked) = chosen.get_mut(file) {
            picked.retain(|id| !names_job(id, job));
        }
    }

    for (file, picked) in &chosen {
        if picked.is_empty() && moved.iter().any(|m| m.starts_with(&format!("{file}/"))) {
            return Err(format!(
                "every job of {file} is on the host runner, so act would run nothing. Remove it from workflows"
            ));
        }

        let graph = graphs.get(file).into_iter().flatten();

        for (id, needs) in graph.filter(|(id, _)| picked.contains(id)) {
            if let Some(need) = needs.iter().find(|n| !picked.contains(*n)) {
                return Err(format!(
                    "act job {file}/{id} needs {need}, which a jobs entry puts on the host runner"
                ));
            }
        }
    }

    Ok(chosen)
}

/// A workflow file's jobs, each with the ids of the jobs it needs
type Graphs = BTreeMap<String, Vec<(String, Vec<String>)>>;

/// Whether `job` names job `id`, itself or as one leg `<job>__<n>` of its matrix
fn names_job(id: &str, job: &str) -> bool {
    let leg = id
        .strip_prefix(job)
        .and_then(|rest| rest.strip_prefix("__"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    id == job || leg
}

/// The jobs a run of this plan reports, from the base's workflows
pub fn jobs(plan: &Plan) -> Result<Jobs, String> {
    expected_jobs(&plan.config, |file| {
        plan.workflows
            .iter()
            .find(|(path, _)| file_name(path) == file)
            .map(|(_, body)| String::from_utf8_lossy(body).into_owned())
    })
}

/// A required workflow as it runs: prepared for the runner, then wrapped in markers
fn as_run_body(
    runner: &dyn Runner,
    body: &[u8],
    file: &str,
    nonce: &str,
) -> Result<Vec<u8>, String> {
    let prepared = runner.prepare(&String::from_utf8_lossy(body), file)?;
    Ok(workflow::add_markers(&prepared, nonce)?.into_bytes())
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Whether a commit message asks the hooks to start no run, with GitHub's own markers
pub fn message_skips(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "[skip ci]",
        "[ci skip]",
        "[skip shipcheck]",
        "[shipcheck skip]",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

/// `SHIPCHECK_SKIP` set to anything but empty, `0` or `false`
fn env_skips(value: Option<&str>) -> bool {
    value.is_some_and(|v| !matches!(v.trim().to_lowercase().as_str(), "" | "0" | "false"))
}

/// A skip only stops the hooks from starting a run. It never produces a result, so a required check
/// stays missing until the commit runs
pub fn skip_requested(plan: &Plan) -> bool {
    if env_skips(std::env::var("SHIPCHECK_SKIP").ok().as_deref()) {
        return true;
    }

    git(&plan.repo, &["log", "-1", "--format=%B", &plan.sha]).is_ok_and(|m| message_skips(&m))
}

/**
 * The key and base commit of the last recorded run of `tree` whose key `has` something. `logs` and
 * `status` read it when the key under origin/main has nothing, such as after a trial with
 * `--base <branch>`, which keys the run by that branch's shipcheck.toml
 **/
pub fn last_run(
    home: &Path,
    repo: &str,
    tree: &str,
    has: impl Fn(&str) -> bool,
) -> Option<(String, String)> {
    history::runs(home, repo, usize::MAX)
        .into_iter()
        .rev()
        .find(|r| r["tree"] == tree && r["key"].as_str().is_some_and(&has))
        .map(|r| {
            let field = |k: &str| r[k].as_str().unwrap_or_default().to_string();
            (field("key"), field("base"))
        })
}

pub fn cached(key: &str) -> Option<Entry> {
    let text = fs::read_to_string(home().join("cache").join(format!("{key}.json"))).ok()?;
    let entry: Entry = serde_json::from_str(&text).ok()?;
    // A result copied to another key's file is not that key's result
    (entry.key == key).then_some(entry)
}

/// The result for a plan without running it: a pass when `paths` excludes the change, else the cache
pub fn lookup(plan: &Plan) -> Option<Entry> {
    let mut entry = skipped(plan).or_else(|| cached(&plan.key))?;
    if !holds(plan, &entry) {
        return None;
    }

    // A cached result can come from another commit with the same tree, the notes must be this one's
    entry.ci_changes.clone_from(&plan.ci_changes);
    Some(entry)
}

/// Fails a result whose pass does not hold, every workflow too, so each job check fails beside `shipcheck`
pub fn fail_closed(entry: &mut Entry) {
    entry.passed = false;

    for wf in &mut entry.workflows {
        wf.passed = false;
    }
}

/**
 * What a check shows of a stored result must hold before it is posted again, or the result does not
 * count and the commit runs: the plan's tree, a base on the plan's base, and each log hashing to its
 * digest. The key holds the tree, still the stored file is checked, never trusted
 **/
pub fn holds(plan: &Plan, entry: &Entry) -> bool {
    let on_base = Command::new("git")
        .arg("-C")
        .arg(&plan.repo)
        .args(["merge-base", "--is-ancestor", &entry.base, &plan.base])
        .status()
        .is_ok_and(|s| s.success());
    entry.tree == plan.tree && on_base && logs_hold(entry)
}

/// Whether the logs a result names are still the ones it hashed
fn logs_hold(entry: &Entry) -> bool {
    entry.workflows.iter().all(|wf| {
        fs::read(log_dir(&entry.key).join(format!("{}.log", wf.file)))
            .is_ok_and(|log| cache::hex(&Sha256::digest(&log)) == wf.log_sha256)
    })
}

/// A failed result whose passed checks and jobs a full run keeps, and where its logs are
struct Previous {
    entry: Entry,
    logs: PathBuf,
}

/// Where a branch's last full result is named, so its next tree knows what already passed
fn branch_link(plan: &Plan) -> Option<PathBuf> {
    let branch = plan.branch.as_ref()?;
    let id = repo_id(&plan.repo).ok()?;
    let name = cache::hex(&Sha256::digest(branch.as_bytes()));
    Some(home().join("branches").join(id).join(name))
}

/// Whose failed result a run keeps the passes of: this tree's own, or also the branch's last one
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reuse {
    Tree,
    Branch,
}

/**
 * This tree's own failed result, or with `Reuse::Branch` the branch's last one when it failed and
 * only its tree differs, the same base config, commands, workflows, versions and images. Its passes
 * are kept
 **/
fn previous(plan: &Plan, reuse: Reuse) -> Option<Previous> {
    if let Some(entry) = cached(&plan.key).filter(|e| !e.passed && holds(plan, e)) {
        return Some(Previous {
            entry,
            logs: log_dir(&plan.key),
        });
    }

    if reuse == Reuse::Tree {
        return None;
    }

    let link: serde_json::Value =
        serde_json::from_slice(&fs::read(branch_link(plan)?).ok()?).ok()?;
    let key = link["key"].as_str()?;

    if link["inputs"].as_str() != Some(plan.inputs.as_str()) || key == plan.key {
        return None;
    }

    let entry = cached(key).filter(|e| !e.passed && e.skipped.is_none() && logs_hold(e))?;
    Some(Previous {
        entry,
        logs: log_dir(key),
    })
}

/// Names this result as the branch's last, for the next tree's run
fn link_branch(plan: &Plan) {
    if let Some(path) = branch_link(plan) {
        let body = serde_json::json!({"key": plan.key, "inputs": plan.inputs, "tree": plan.tree});
        let _ = path.parent().map(fs::create_dir_all);
        let _ = fs::write(path, body.to_string());
    }
}

/**
 * A pass with nothing run when no file changed since the merge base with `base` matches `paths`,
 * like a path filter on a workflow. It depends on where `base` is, which the cache key does not
 * hold, so it is worked out again each time instead of cached. Any error means run
 **/
pub fn skipped(plan: &Plan) -> Option<Entry> {
    let filter = plan.config.path_filter().ok()??;
    let range = format!("{}...{}", plan.base, plan.sha);
    // Raw NUL-separated names, and both sides of a rename, so no quoted or moved path slips past
    let changed = git_raw(
        &plan.repo,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            &range,
        ],
    )
    .ok()?;

    for name in changed.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = std::str::from_utf8(name).ok()?;

        if filter.is_match(name) {
            return None;
        }
    }

    Some(Entry {
        key: plan.key.clone(),
        sha: plan.sha.clone(),
        tree: plan.tree.clone(),
        base: plan.base.clone(),
        passed: true,
        workflows: Vec::new(),
        finished_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        skipped: Some(format!(
            "no change under paths ({})",
            plan.config.paths.join(", ")
        )),
        ci_changes: plan.ci_changes.clone(),
    })
}

/// The values in a repository's secrets file, longest first, to redact from anything posted. Each
/// line of a value with `\n` escapes is a value too, since a log prints it line by line
pub fn secret_values(repo_id: &str) -> Result<Vec<String>, String> {
    let text = secret::read(&secrets_file(repo_id))?.unwrap_or_default();
    Ok(parse_secrets(&String::from_utf8_lossy(&text)))
}

fn parse_secrets(text: &str) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();

    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let Some((_, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']);
        values.push(value.to_string());
        values.extend(value.split("\\n").map(|v| v.trim().to_string()));
    }

    values.retain(|v| !v.is_empty());
    values.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    values.dedup();
    values
}

fn nonce(key: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let h = Sha256::digest(format!("{key}{}{}", now.as_nanos(), std::process::id()));
    cache::hex(&h[..8])
}

/**
 * Builds embed absolute paths, such as cargo's `CARGO_BIN_EXE_*`, and a warm `target/` cloned to a
 * new path still points at the old one. So runs of a repo reuse a few fixed paths, one per slot,
 * held by a lock for the length of a run. The directory itself is still new for every run
 **/
fn claim_slot(slots: &Path) -> Result<(PathBuf, fs::File), String> {
    for n in 0..64 {
        let lock = fs::File::create(slots.join(format!("{n}.lock"))).map_err(|e| e.to_string())?;

        if lock.try_lock().is_ok() {
            return Ok((slots.join(n.to_string()), lock));
        }
    }

    Err("all 64 run slots of this repository are busy".into())
}

/// Held while one tree runs, so a second request for it waits and then reads the cached result
pub fn lock_key(key: &str) -> Result<fs::File, String> {
    let dir = home().join("locks");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let lock = fs::File::create(dir.join(format!("{key}.lock"))).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    Ok(lock)
}

/**
 * Takes the tree's lock, and when another run held it, the time this one began to wait. A result
 * stored after that is the one this run waited for, see [`reusable`]
 **/
pub fn lock_tree(key: &str) -> Result<(fs::File, Option<u64>), String> {
    let waited = running(key).then(unix_now);
    Ok((lock_key(key)?, waited))
}

/// A stored pass, or any result a run stored while this one waited for it, so a failure that just
/// ran is not run again at once. A failure found later runs again, so a flaky one cannot stick
pub fn reusable(plan: &Plan, waited: Option<u64>) -> Option<Entry> {
    lookup(plan).filter(|e| e.passed || waited.is_some_and(|t| e.finished_unix >= t))
}

/// The one key under `home` that starts with `id`, a run's id, among the trees that ran or run
pub fn find_key(home: &Path, id: &str) -> Option<String> {
    if id.len() < 6 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }

    let mut keys = BTreeSet::new();

    for (dir, ext) in [("locks", ".lock"), ("cache", ".json")] {
        for entry in fs::read_dir(home.join(dir)).into_iter().flatten().flatten() {
            let name = entry.file_name();

            if let Some(key) = name.to_str().and_then(|n| n.strip_suffix(ext))
                && key.starts_with(id)
            {
                keys.insert(key.to_string());
            }
        }
    }

    // An id that names two trees names none
    (keys.len() == 1).then(|| keys.pop_first()).flatten()
}

/// Whether a run of this key is going right now on this machine, without waiting for it
pub fn running(key: &str) -> bool {
    let path = home().join("locks").join(format!("{key}.lock"));

    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .is_ok_and(|lock| lock.try_lock().is_err())
}

/// The machine's run slots: `SHIPCHECK_MAX_RUNS`, or a sixth of the CPUs, since three brain runs on
/// 14 CPUs left none idle and failed its timing tests
pub fn max_runs() -> usize {
    std::env::var("SHIPCHECK_MAX_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() / 6))
        .max(1)
}

/// A job that ended, sent while the run goes on so its check can complete before the others
#[derive(Debug)]
pub struct JobDone {
    pub file: String,
    pub job: String,
    pub state: String,
    pub times: [u64; 2],
}

/// The line of a job that ended, with the time it took when it ran
fn ended(file: &str, job: &str, state: &str, time: [u64; 2]) -> String {
    match time {
        [0, _] => format!("{file}/{job}: {state}"),
        [start, end] => format!(
            "{file}/{job}: {state}, {}",
            clock(end.saturating_sub(start))
        ),
    }
}

/// Sends a job that ended to the run's poster, when the run has one
fn job_done(ctx: &Ctx<'_>, file: &str, job: &str, state: &str, times: [u64; 2]) {
    if let Some(tx) = ctx.jobs {
        let _ = tx.send(JobDone {
            file: file.to_string(),
            job: job.to_string(),
            state: state.to_string(),
            times,
        });
    }
}

/// Whether this process waits for a run slot, which its heartbeat counts as progress
pub static WAITING_FOR_SLOT: AtomicBool = AtomicBool::new(false);

/// Whether this process holds a run slot, so its checks may show it running
pub static HAS_SLOT: AtomicBool = AtomicBool::new(false);

/// The seconds this process waited for its run slot, which its record keeps apart from the run's time
pub static QUEUED_S: AtomicU64 = AtomicU64::new(0);

/// A run's place in the queue for a machine run slot, a file it holds locked and removes when it goes
struct Ticket {
    path: PathBuf,
    _lock: fs::File,
}

impl Ticket {
    /**
     * Named `<priority>-<arrival>-<pid>`, so the names sort into the queue's order. It is locked under
     * a temporary name and renamed into the queue, so no other run sees it unlocked and removes it.
     * It holds what `shipcheck queue` prints of the run
     **/
    fn take(queue: &Path, priority: u8, run: &queue::Run) -> Result<Ticket, String> {
        fs::create_dir_all(queue).map_err(|e| format!("{}: {e}", queue.display()))?;
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!("{priority}-{at:024}-{}", std::process::id());
        let temp = queue.join(format!(".{name}"));
        let lock = fs::File::create(&temp).map_err(|e| format!("{}: {e}", temp.display()))?;
        lock.try_lock()
            .map_err(|e| format!("{}: {e}", temp.display()))?;
        fs::write(&temp, serde_json::json!(run).to_string())
            .map_err(|e| format!("{}: {e}", temp.display()))?;
        let path = queue.join(name);
        fs::rename(&temp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Ticket { path, _lock: lock })
    }

    fn name(&self) -> &std::ffi::OsStr {
        self.path.file_name().unwrap_or_default()
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// What one look at the queue found: a slot to hold, or how many live runs are ahead
enum QueueStep {
    Slot(fs::File, usize),
    Ahead(usize),
}

/**
 * The live tickets ahead of `ticket`, removing those of runs that died. Only the head takes a slot,
 * so slots go out in the queue's order
 **/
fn queue_step(dir: &Path, ticket: &Ticket, slots: usize) -> Result<QueueStep, String> {
    let queue = dir.join("queue");
    let mine = ticket.name();
    let mut names: Vec<_> = fs::read_dir(&queue)
        .map_err(|e| format!("{}: {e}", queue.display()))?
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|n| !n.to_string_lossy().starts_with('.'))
        .collect();
    names.sort();
    let mut ahead = 0;

    for name in names.iter().take_while(|n| n.as_os_str() < mine) {
        let path = queue.join(name);

        // A ticket nobody holds belongs to a run that is gone
        match fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) if f.try_lock().is_ok() => {
                let _ = fs::remove_file(&path);
            }
            Ok(_) => ahead += 1,
            Err(_) => {}
        }
    }

    if ahead > 0 {
        return Ok(QueueStep::Ahead(ahead));
    }

    for n in 0..slots {
        let lock = fs::File::create(dir.join(format!("{n}.lock"))).map_err(|e| e.to_string())?;

        if lock.try_lock().is_ok() {
            return Ok(QueueStep::Slot(lock, n));
        }
    }

    Ok(QueueStep::Ahead(0))
}

/**
 * Waits in the machine's queue for a run slot. `shipcheck run`, which an agent waits on, goes ahead
 * of the runs the hooks and the daemon start, and each kind goes in the order it came
 **/
fn claim_runner(plan: &Plan, say: &mut dyn FnMut(&str)) -> Result<(fs::File, queue::Note), String> {
    let dir = home().join("runners");
    let trigger = std::env::var("SHIPCHECK_TRIGGER").ok();
    let priority = u8::from(trigger.is_some());
    let run = queue::Run {
        repo: repo_id(&plan.repo).unwrap_or_default(),
        key: plan.key.clone(),
        path: plan.repo.display().to_string(),
        branch: plan.branch.clone(),
        sha: plan.sha.clone(),
        trigger: trigger.unwrap_or_else(|| "cli".into()),
        pid: std::process::id(),
        queued_unix: unix_now(),
        ..queue::Run::default()
    };
    let ticket = Ticket::take(&dir.join("queue"), priority, &run)?;
    let mut told = None;

    loop {
        match queue_step(&dir, &ticket, max_runs())? {
            QueueStep::Slot(lock, n) => {
                WAITING_FOR_SLOT.store(false, Ordering::Relaxed);
                HAS_SLOT.store(true, Ordering::Relaxed);
                let now = unix_now();
                QUEUED_S.store(now.saturating_sub(run.queued_unix), Ordering::Relaxed);

                if told.is_some() {
                    say(&format!(
                        "started after {}s in the queue",
                        now.saturating_sub(run.queued_unix)
                    ));
                }

                return Ok((lock, queue::Note::start(&dir, n, run, now)));
            }
            QueueStep::Ahead(ahead) => {
                if told != Some(ahead) {
                    say(&match ahead {
                        0 => "queued for a run slot, every slot is busy (position 1)".to_string(),
                        1 => "queued behind 1 run (position 2)".to_string(),
                        n => format!("queued behind {n} runs (position {})", n + 1),
                    });
                    WAITING_FOR_SLOT.store(true, Ordering::Relaxed);
                    told = Some(ahead);
                }
            }
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Runs every required workflow of the plan, each job in its own copy-on-write clone, and caches
/// the result
pub fn run(plan: &Plan, say: &mut dyn FnMut(&str)) -> Result<Entry, String> {
    run_reporting(plan, say, None)
}

/// [`run`], sending each job to `jobs` as it ends, so its check can complete before the others
pub fn run_reporting(
    plan: &Plan,
    say: &mut dyn FnMut(&str),
    jobs: Option<&mpsc::Sender<JobDone>>,
) -> Result<Entry, String> {
    run_with(plan, say, jobs, Reuse::Tree)
}

/**
 * A full run that keeps the checks and jobs that passed in the branch's last failed result, on any
 * tree, and runs the rest. Only when `keep_passed` on main allows it, since a change that breaks a
 * kept check then passes
 **/
pub fn run_only_failed(plan: &Plan, say: &mut dyn FnMut(&str)) -> Result<Entry, String> {
    if !plan.config.keep_passed {
        return Err(
            "--only-failed needs `keep_passed = true` in shipcheck.toml on main, run `shipcheck run` for every check".into(),
        );
    }

    run_with(plan, say, None, Reuse::Branch)
}

fn run_with(
    plan: &Plan,
    say: &mut dyn FnMut(&str),
    jobs: Option<&mpsc::Sender<JobDone>>,
    reuse: Reuse,
) -> Result<Entry, String> {
    let entry = execute(plan, None, say, jobs, reuse)?;

    // Docker or the sandbox failed before the code ran, or a clone was wrong, so there is no verdict to keep
    if entry.could_not_run() || entry.wrong_tree() {
        return Ok(entry);
    }

    let cache_dir = home().join("cache");
    fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&entry).map_err(|e| e.to_string())?;
    fs::write(cache_dir.join(format!("{}.json", plan.key)), json).map_err(|e| e.to_string())?;
    link_branch(plan);
    Ok(entry)
}

/// Runs only the named jobs and workflows and the jobs they need. The result is never cached, so
/// it is never the check, and its logs go to [`partial_log_dir`]
pub fn run_partial(
    plan: &Plan,
    jobs: &[String],
    workflows: &[String],
    say: &mut dyn FnMut(&str),
) -> Result<Entry, String> {
    execute(plan, Some((jobs, workflows)), say, None, Reuse::Tree)
}

/// The jobs to run per workflow file: every job of each named workflow, each job whose id or
/// matrix leg matches a name, or `<file>/<job>` for one file's, and every job they need
/// Each job as `--job` takes it: by name, as `<file>/<job>`, and a matrix job's legs
fn job_names(graphs: &Graphs) -> Vec<String> {
    let mut out = Vec::new();

    for (file, graph) in graphs {
        let mut legs: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

        for (id, _) in graph {
            let base = id
                .rsplit_once("__")
                .filter(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .map_or(id.as_str(), |(b, _)| b);
            let entry = legs.entry(base).or_default();

            if base != id {
                entry.push(id);
            }
        }

        for (base, of) in legs {
            out.push(if of.is_empty() {
                format!("{base} ({file}/{base})")
            } else {
                format!("{base} ({file}/{base}, legs {})", of.join(", "))
            });
        }
    }

    out
}

pub fn select(
    graphs: &Graphs,
    jobs: &[String],
    workflows: &[String],
) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    let mut chosen: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for wf in workflows {
        let name = file_name(wf);
        let graph = graphs.get(&name).ok_or_else(|| {
            format!(
                "no workflow {name} in shipcheck.toml's workflows: {}",
                graphs.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        chosen
            .entry(name)
            .or_default()
            .extend(graph.iter().map(|(j, _)| j.clone()));
    }

    for job in jobs {
        let mut found = false;

        let (in_file, name) = match job.split_once('/') {
            Some((file, name)) => (Some(file), name),
            None => (None, job.as_str()),
        };

        for (file, graph) in graphs
            .iter()
            .filter(|(f, _)| in_file.is_none_or(|i| i == *f))
        {
            for (id, _) in graph {
                if names_job(id, name) {
                    chosen.entry(file.clone()).or_default().insert(id.clone());
                    found = true;
                }
            }
        }

        if !found {
            return Err(format!(
                "no job {job}, the jobs are: {}",
                job_names(graphs).join(", ")
            ));
        }
    }

    for (file, picked) in &mut chosen {
        let needs: BTreeMap<_, _> = graphs[file].iter().cloned().collect();
        let mut todo: Vec<String> = picked.iter().cloned().collect();

        while let Some(job) = todo.pop() {
            for need in needs.get(&job).into_iter().flatten() {
                if picked.insert(need.clone()) {
                    todo.push(need.clone());
                }
            }
        }
    }

    Ok(chosen)
}

/// The host checks `--job` names, under the one workflow host mode has. `--workflow` names none
pub fn select_host(
    checks: &BTreeMap<String, Check>,
    jobs: &[String],
    workflows: &[String],
) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    if !workflows.is_empty() {
        return Err(
            "--workflow picks workflows, host mode runs [host] checks: use --job <check>".into(),
        );
    }

    if let Some(missing) = jobs.iter().find(|j| !checks.contains_key(*j)) {
        return Err(format!(
            "no check {missing}, the checks are: {}",
            checks.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    Ok(BTreeMap::from([(
        HOST.to_string(),
        jobs.iter().cloned().collect(),
    )]))
}

type Selection<'a> = Option<(&'a [String], &'a [String])>;

#[expect(
    clippy::too_many_lines,
    reason = "one run, from its slot to its result, read top to bottom"
)]
fn execute(
    plan: &Plan,
    only: Selection<'_>,
    say: &mut dyn FnMut(&str),
    jobs: Option<&mpsc::Sender<JobDone>>,
    reuse: Reuse,
) -> Result<Entry, String> {
    let (_runner, note) = claim_runner(plan, say)?;
    let home = home();
    // Results, logs, secrets and the App key live here, other users of the machine read none of it
    let private = <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700);
    fs::create_dir_all(&home).map_err(|e| format!("{}: {e}", home.display()))?;
    fs::set_permissions(&home, private).map_err(|e| format!("{}: {e}", home.display()))?;
    exclude_from_backup(&home);
    let id = repo_id(&plan.repo)?;
    let templates = home.join("templates").join(&id);
    let slots = home.join("runs").join(&id);
    fs::create_dir_all(&slots).map_err(|e| e.to_string())?;
    let (slot, slot_file) = claim_slot(&slots)?;
    let runners = Runners::new(&plan.config, &plan.repo);

    for runner in runners.all() {
        runner.preflight()?;
    }

    prune(&home, SystemTime::now());
    templates::prune(&home, SystemTime::now(), &slot.with_extension("lock"));
    // A host check that outlived a crashed run could otherwise edit this slot's next checkout
    host::reap_slot(&slot)?;
    {
        let _git = git_lock(&id)?;
        remove_checkouts(plan, &runners, &slot);
    }

    // Before one checkout per job, the template was a single clone at this path
    if templates.join(".git").exists() {
        let _ = fs::remove_dir_all(&templates);
    }

    let nonce = nonce(&plan.key);
    let act_files = plan.config.act_files();
    let mut bodies = BTreeMap::new();

    for (path, body) in &plan.workflows {
        let file = file_name(path);
        let body = if act_files.contains(&file) {
            as_run_body(runners.for_file(&file)?, body, &file, &nonce)?
        } else {
            body.clone()
        };
        bodies.insert(file, body);
    }

    let mut graphs = Graphs::new();

    for file in &act_files {
        let body = String::from_utf8_lossy(&bodies[file]).into_owned();
        graphs.insert(file.clone(), workflow::job_needs(&body)?);
    }

    let acts = act_jobs(&plan.config, &graphs)?;

    // A partial run picks among the jobs a full run would run
    for (file, graph) in &mut graphs {
        graph.retain(|(id, _)| acts.get(file).is_some_and(|picked| picked.contains(id)));
    }

    let checks = &plan.config.host.checks;
    let only = match only {
        Some((jobs, workflows)) if !plan.config.acts() => {
            Some(select_host(checks, jobs, workflows)?)
        }
        Some((jobs, workflows)) if !plan.config.hosts() => Some(select(&graphs, jobs, workflows)?),
        Some((jobs, workflows)) => {
            let (on_host, on_act): (Vec<String>, Vec<String>) =
                jobs.iter().cloned().partition(|j| checks.contains_key(j));
            let mut picked = select(&graphs, &on_act, workflows)?;

            if !on_host.is_empty() {
                picked.extend(select_host(checks, &on_host, &[])?);
            }

            Some(picked)
        }
        None => None,
    };
    let logs = if only.is_some() {
        let dir = partial_log_dir(&plan.key);
        let _ = fs::remove_dir_all(&dir);
        dir
    } else {
        log_dir(&plan.key)
    };
    let slots = Slots::new(plan.config.max_parallel());
    let ctx = Ctx {
        note: &note,
        slot_lock: slot_file.as_raw_fd(),
        plan,
        runners: &runners,
        acts: &acts,
        repo_id: &id,
        slot: &slot,
        templates: &templates,
        bodies: &bodies,
        nonce: &nonce,
        logs,
        slots: &slots,
        jobs,
        previous: if only.is_none() {
            previous(plan, reuse)
        } else {
            None
        },
        only: only.as_ref(),
        base_tree: std::sync::OnceLock::new(),
    };

    let results = run_all(&ctx, say)?;

    {
        let _git = git_lock(&id)?;
        prune_worktrees(&plan.repo);
    }

    for line in runners.all().flat_map(super::runner::Runner::prune) {
        say(&line);
    }

    let entry = Entry {
        key: plan.key.clone(),
        sha: plan.sha.clone(),
        tree: plan.tree.clone(),
        base: plan.base.clone(),
        passed: results.iter().all(|r| r.passed),
        workflows: results,
        finished_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        skipped: None,
        ci_changes: plan.ci_changes.clone(),
    };
    Ok(entry)
}

/// One host check as `init` tried it
#[derive(Debug)]
pub struct Trial {
    pub passed: bool,
    pub log: String,
    pub secs: u64,
}

/**
 * Runs each host check of `config` once on HEAD, as `run` runs it: the same jail, a new clone, at
 * most `max_parallel` at once. For `init`, which writes only checks that passed. Nothing is cached,
 * posted or recorded, the logs are removed, and `versions` is not run, the caller tries each
 **/
pub fn trial(
    repo: &Path,
    mut config: RepoConfig,
    say: &mut dyn FnMut(&str),
) -> Result<BTreeMap<String, Trial>, String> {
    let repo = PathBuf::from(git(repo, &["rev-parse", "--show-toplevel"])?);
    let sha = git(&repo, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let tree = git(&repo, &["rev-parse", &format!("{sha}^{{tree}}")])?;
    config.timeout_minutes = config.timeout_minutes.min(10);
    config.retries = 0;
    // Only the host checks are tried, act's jobs need Docker and their own runs
    config.jobs.clear();
    config.workflows.clear();
    let names: Vec<String> = config.host.checks.keys().cloned().collect();
    let plan = Plan {
        key: format!("trial-{}", nonce(&tree)),
        repo,
        sha: sha.clone(),
        tree,
        base: sha,
        config_text: Vec::new(),
        config,
        workflows: BTreeMap::new(),
        ci_changes: Vec::new(),
        tools: String::new(),
        branch: None,
        inputs: String::new(),
    };
    let entry = execute(&plan, None, say, None, Reuse::Tree);
    let logs = log_dir(&plan.key);
    let read = |name: &str| {
        let log = fs::read(logs.join(HOST).join(format!("{name}.log"))).unwrap_or_default();
        printable(&String::from_utf8_lossy(&log))
    };
    let result = entry.map(|entry| {
        let host = entry.workflows.first();
        names
            .iter()
            .map(|name| {
                let passed = host.is_some_and(|h| h.jobs.get(name).is_some_and(|s| s == "success"));
                let secs = host
                    .and_then(|h| h.times.get(name))
                    .map_or(0, |[start, end]| end.saturating_sub(*start));
                (
                    name.clone(),
                    Trial {
                        passed,
                        log: read(name),
                        secs,
                    },
                )
            })
            .collect()
    });
    let _ = fs::remove_dir_all(&logs);
    result
}

/**
 * The host checks, as one result, and every required workflow at once, as on GitHub, with
 * `max_parallel` shared by all their jobs
 **/
fn run_all(ctx: &Ctx<'_>, say: &mut dyn FnMut(&str)) -> Result<Vec<WorkflowResult>, String> {
    let plan = ctx.plan;
    let host = ctx.runners.has_host() && ctx.only.is_none_or(|o| o.contains_key(HOST));
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<String>();
        let host = host.then(|| {
            let tx = tx.clone();
            s.spawn(move || {
                run_host(ctx, &mut |line: &str| {
                    let _ = tx.send(line.to_string());
                })
            })
        });
        let handles: Vec<_> = host
            .into_iter()
            .chain(
                plan.config
                    .act_files()
                    .into_iter()
                    .filter(|f| ctx.acts.contains_key(f))
                    .filter(|f| ctx.only.is_none_or(|o| o.contains_key(f)))
                    .map(|file| {
                        let tx = tx.clone();
                        s.spawn(move || {
                            run_workflow(ctx, &file, &mut |line: &str| {
                                let _ = tx.send(line.to_string());
                            })
                        })
                    }),
            )
            .collect();
        drop(tx);

        for line in rx {
            say(&line);
        }

        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err("a workflow thread panicked".into()))
            })
            .collect::<Result<Vec<_>, String>>()
    })
}

/**
 * Runs every `[host]` check at once, up to `max_parallel`, each in its own clone like a job, as one
 * result named [`HOST`] whose jobs are the checks. Nothing from the base is written into the clone,
 * since the commands come from the plan and no workflow runs
 **/
/// What a run and doctor say about a `sandbox = false` check this machine does not trust
pub const UNTRUSTED: &str = "this repository asks to run it with sandbox = false, so it runs in the sandbox here. Run `shipcheck trust` to allow it if you trust the repository";

/// What a run and doctor say about a check with `sandbox = false`
pub const UNSANDBOXED: &str = "runs outside the sandbox, as `sandbox = false` on the base says, with this user's full access to the home, its keys and tokens. For trusted repositories only";

#[expect(
    clippy::too_many_lines,
    reason = "one host run, from the checks to its result, read top to bottom"
)]
fn run_host(ctx: &Ctx<'_>, say: &mut dyn FnMut(&str)) -> Result<WorkflowResult, String> {
    let checks = &ctx.plan.config.host.checks;
    let (templates, checkouts) = (ctx.templates.join(HOST), ctx.slot.join(HOST));
    fs::create_dir_all(&templates).map_err(|e| e.to_string())?;
    fs::create_dir_all(&checkouts).map_err(|e| e.to_string())?;
    fs::create_dir_all(ctx.logs.join(HOST)).map_err(|e| e.to_string())?;
    git_cow::can_clone(&templates, &checkouts);
    let deadline = Instant::now() + Duration::from_secs(ctx.plan.config.timeout_minutes * 60);

    for (name, c) in checks {
        if !c.sandbox {
            say(&format!("{HOST}/{name}: {UNSANDBOXED}"));
        } else if c.untrusted {
            say(&format!("{HOST}/{name}: {UNTRUSTED}"));
        }
    }

    // A full run of a tree that failed keeps the checks that passed on it, only the others run again
    let kept = if ctx.only.is_none() {
        kept_checks(ctx)
    } else {
        BTreeMap::new()
    };

    for (name, (_, time)) in &kept {
        say(&kept_line(ctx, HOST, name));
        job_done(ctx, HOST, name, "success", *time);
    }

    let kept_trees: BTreeMap<String, String> = kept_from(ctx)
        .map_or_default(|tree| kept.keys().map(|n| (n.clone(), tree.to_string())).collect());

    let mut runs: Vec<(String, Result<JobRun, String>, [u64; 2])> = std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<Msg>();
        let handles: Vec<_> = checks
            .keys()
            .filter(|name| {
                ctx.only
                    .is_none_or(|o| o.get(HOST).is_some_and(|picked| picked.contains(*name)))
                    && !kept.contains_key(*name)
            })
            .map(|name| {
                let tx = tx.clone();
                s.spawn(move || {
                    let (none, unused) = (BTreeMap::new(), ctx.slot.join(HOST));
                    let start = unix_now();
                    let run = run_job(ctx, HOST, name, &none, deadline, &unused, &tx);
                    let time = [start, unix_now()];
                    let state = host_state(&run);

                    if let Err(e) = &run {
                        let _ = tx.send(Msg::Say(format!("{HOST}/{name}: {e}")));
                    }

                    // Said as the check ends, so the output shows where the time went
                    let _ = tx.send(Msg::Say(ended(HOST, name, state, time)));
                    job_done(ctx, HOST, name, state, time);
                    (name.clone(), run, time)
                })
            })
            .collect();
        drop(tx);

        for msg in rx {
            if let Msg::Say(line) = msg {
                say(&line);
            }
        }

        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    (String::new(), Err("a check thread panicked".into()), [0; 2])
                })
            })
            .collect()
    });
    let reused: BTreeSet<String> = kept.keys().cloned().collect();
    runs.extend(
        kept.into_iter()
            .map(|(name, (run, time))| (name, Ok(run), time)),
    );
    runs.sort_by(|a, b| a.0.cmp(&b.0));

    let (mut states, mut times) = (BTreeMap::new(), BTreeMap::new());
    let (mut exit, mut attempts, mut log) = (Some(0), 0, String::new());
    let (mut summaries, mut shards) = (BTreeMap::new(), BTreeMap::new());

    for (name, run, time) in runs {
        times.insert(name.clone(), time);
        let state = host_state(&run);
        let state = match run {
            Ok(run) => {
                attempts = attempts.max(run.attempts);
                log.push_str(&run.log);

                if let Some(info) = run.shard {
                    shards.insert(name.clone(), info);
                }

                if !run.summary.is_empty() {
                    summaries.insert(name.clone(), run.summary);
                }

                if exit == Some(0) && run.exit != Some(0) {
                    exit = run.exit;
                }

                state
            }
            Err(e) => {
                exit = exit.and(Some(1));

                // A check that ran said its error as it ended, a thread that panicked said nothing
                if name.is_empty() {
                    say(&format!("{HOST}/{name}: {e}"));
                }

                state
            }
        };

        if reused.contains(&name) || name.is_empty() {
            say(&format!("{HOST}/{name}: {state}"));
        }

        states.insert(name, state.to_string());
    }

    fs::write(ctx.logs.join(format!("{HOST}.log")), &log).map_err(|e| e.to_string())?;
    let passed = !states.is_empty() && states.values().all(|s| s == "success");
    say(&format!(
        "host checks: {}",
        if passed { "passed" } else { "failed" }
    ));

    Ok(WorkflowResult {
        file: HOST.to_string(),
        exit,
        attempts,
        log_sha256: cache::hex(&Sha256::digest(log.as_bytes())),
        jobs: states,
        passed,
        shards,
        kept: kept_trees,
        times,
        summaries,
    })
}

/**
 * The host checks that passed in the failed result this full run keeps, with their logs copied into
 * this run. A pass from a run of the same inputs is kept, a failure always runs again
 **/
fn kept_checks(ctx: &Ctx<'_>) -> BTreeMap<String, (JobRun, [u64; 2])> {
    let Some(prev) = &ctx.previous else {
        return BTreeMap::new();
    };
    let Some(wf) = prev.entry.workflows.iter().find(|w| w.file == HOST) else {
        return BTreeMap::new();
    };

    wf.jobs
        .iter()
        .filter(|(name, state)| {
            *state == "success" && ctx.plan.config.host.checks.contains_key(*name)
        })
        .filter_map(|(name, _)| {
            let log = keep_log(ctx, &prev.logs, HOST, name)?;
            let run = JobRun {
                exit: Some(0),
                skipped: false,
                attempts: 1,
                outputs: BTreeMap::new(),
                unsupported: Vec::new(),
                log,
                shard: wf.shards.get(name).cloned(),
                summary: wf.summaries.get(name).cloned().unwrap_or_default(),
            };
            Some((
                name.clone(),
                (run, wf.times.get(name).copied().unwrap_or_default()),
            ))
        })
        .collect()
}

/// A kept job's log from the result it passed in, copied into this run's logs
fn keep_log(ctx: &Ctx<'_>, from: &Path, file: &str, job: &str) -> Option<String> {
    let name = format!("{job}.log");
    let log = fs::read_to_string(from.join(file).join(&name)).ok()?;

    if from != ctx.logs {
        fs::create_dir_all(ctx.logs.join(file)).ok()?;
        fs::write(ctx.logs.join(file).join(&name), &log).ok()?;
    }

    Some(log)
}

/**
 * The jobs of a workflow a run keeps from an earlier result's states: each that passed, unless a job
 * that runs again needs it, since a kept job's outputs are not stored
 **/
fn keep_jobs(graph: &[(String, Vec<String>)], states: &BTreeMap<String, String>) -> Vec<String> {
    let mut again: BTreeSet<&str> = graph
        .iter()
        .filter(|(job, _)| states.get(job).is_none_or(|s| s != "success"))
        .map(|(job, _)| job.as_str())
        .collect();
    let mut grew = true;

    while grew {
        grew = false;

        for (job, needs) in graph {
            if again.contains(job.as_str()) {
                for need in needs {
                    grew |= again.insert(need.as_str());
                }
            }
        }
    }

    graph
        .iter()
        .map(|(job, _)| job)
        .filter(|job| !again.contains(job.as_str()))
        .cloned()
        .collect()
}

/// The tree a kept job passed on, when it is another tree than this run's
fn kept_from<'a>(ctx: &'a Ctx<'_>) -> Option<&'a str> {
    ctx.previous
        .as_ref()
        .map(|p| p.entry.tree.as_str())
        .filter(|t| *t != ctx.plan.tree)
}

/// What a run says about a job it kept
fn kept_line(ctx: &Ctx<'_>, file: &str, job: &str) -> String {
    match kept_from(ctx) {
        Some(tree) => format!(
            "{file}/{job}: passed on tree {}, kept",
            &tree[..12.min(tree.len())]
        ),
        None => format!("{file}/{job}: passed on this tree already, kept"),
    }
}

/// A host check's state from how its run ended
fn host_state(run: &Result<JobRun, String>) -> &'static str {
    match run {
        Ok(run) => match run.exit {
            Some(0) => "success",
            None => "timed out",
            Some(_) => "failure",
        },
        Err(e) => error_state(e),
    }
}

/// Removes the checkouts and containers a crashed run left in this slot
fn remove_checkouts(plan: &Plan, runners: &Runners, slot: &Path) {
    if !slot.exists() {
        return;
    }

    // The slot itself was the checkout before one checkout per job
    let mut dirs = vec![slot.to_path_buf()];

    for file in fs::read_dir(slot).into_iter().flatten().flatten() {
        dirs.extend(
            fs::read_dir(file.path())
                .into_iter()
                .flatten()
                .flatten()
                .map(|job| job.path()),
        );
    }

    for dir in dirs {
        for runner in runners.all() {
            runner.reclaim(&dir);
        }

        mark_ended(&ended_stamp(slot, &dir));
        let _ = git(
            &plan.repo,
            &["worktree", "remove", "--force", &dir.to_string_lossy()],
        );
    }

    let _ = fs::remove_dir_all(slot);
    prune_worktrees(&plan.repo);
}

/// Ends the state of a failed job with `continue-on-error: true`, which does not fail the workflow
pub const ALLOWED: &str = ", allowed by continue-on-error";

/// The job slots of one run, so all its workflows together start at most `max_parallel` jobs, any number when unset
struct Slots {
    free: Mutex<usize>,
    freed: std::sync::Condvar,
}

impl Slots {
    fn new(count: Option<usize>) -> Self {
        Slots {
            free: Mutex::new(count.map_or(usize::MAX, |n| n.max(1))),
            freed: std::sync::Condvar::new(),
        }
    }

    /// Waits for a free slot, which goes back when the returned guard drops
    fn take(&self) -> SlotGuard<'_> {
        let mut free = self
            .free
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        while *free == 0 {
            free = self
                .freed
                .wait(free)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }

        *free -= 1;
        SlotGuard(self)
    }
}

struct SlotGuard<'a>(&'a Slots);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        *self
            .0
            .free
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        self.0.freed.notify_one();
    }
}

struct Ctx<'a> {
    /// What `shipcheck queue` prints of this run
    note: &'a queue::Note,
    /// The run slot's lock, which a job's reaper inherits
    slot_lock: std::os::fd::RawFd,
    slots: &'a Slots,
    plan: &'a Plan,
    runners: &'a Runners,
    /// The jobs act runs per workflow file, for every job of the run
    acts: &'a Jobs,
    repo_id: &'a str,
    slot: &'a Path,
    templates: &'a Path,
    bodies: &'a BTreeMap<String, Vec<u8>>,
    nonce: &'a str,
    logs: PathBuf,
    /// The jobs a partial run picked per workflow, `None` for every job
    only: Option<&'a BTreeMap<String, BTreeSet<String>>>,
    /// The tree of the remote's default branch, asked once by the first host check
    base_tree: std::sync::OnceLock<Option<String>>,
    /// Where each job goes as it ends, for a run that posts its checks
    jobs: Option<&'a mpsc::Sender<JobDone>>,
    /// The failed result whose passes this full run keeps
    previous: Option<Previous>,
}

enum Msg {
    Say(String),
    Done(String, Result<JobRun, String>),
}

struct JobRun {
    exit: Option<i32>,
    skipped: bool,
    attempts: u32,
    outputs: BTreeMap<String, String>,
    unsupported: Vec<String>,
    log: String,
    shard: Option<ShardRun>,
    summary: String,
}

/// Runs the jobs of one workflow, each once every job in its `needs` has passed, as GitHub does
#[expect(
    clippy::too_many_lines,
    reason = "the job loop of one workflow, its order is the scheduling"
)]
fn run_workflow(
    ctx: &Ctx<'_>,
    file: &str,
    say: &mut dyn FnMut(&str),
) -> Result<WorkflowResult, String> {
    let body = String::from_utf8_lossy(&ctx.bodies[file]).into_owned();
    let mut graph = workflow::job_needs(&body)?;
    graph.retain(|(job, _)| {
        ctx.acts
            .get(file)
            .is_some_and(|picked| picked.contains(job))
    });

    if let Some(picked) = ctx.only.and_then(|o| o.get(file)) {
        graph.retain(|(job, _)| picked.contains(job));
    }

    let allowed = workflow::allowed_failures(&body)?;
    let ifs = job_ifs(&body);
    let skip_on_prs = skipped_on_every_pr(&ifs);
    let runs_anyway = |job: &str| {
        !skip_on_prs.iter().any(|j| names_job(job, j))
            && ifs.iter().any(|(j, c)| {
                names_job(job, j)
                    && ["always()", "failure()", "cancelled()"]
                        .iter()
                        .any(|f| c.contains(f))
            })
    };
    let artifacts = ctx.slot.with_extension("artifacts").join(file);
    let _ = fs::remove_dir_all(&artifacts);
    fs::create_dir_all(&artifacts).map_err(|e| e.to_string())?;
    fs::create_dir_all(ctx.logs.join(file)).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(ctx.plan.config.timeout_minutes * 60);

    // git-cow 0.2.1 names its probe file by process only, so jobs probing at once delete each
    // other's probe. It caches the answer per pair of directories, so one probe here serves them all
    let (templates, checkouts) = (ctx.templates.join(file), ctx.slot.join(file));
    fs::create_dir_all(&templates).map_err(|e| e.to_string())?;
    fs::create_dir_all(&checkouts).map_err(|e| e.to_string())?;
    git_cow::can_clone(&templates, &checkouts);

    let mut states: BTreeMap<String, String> = BTreeMap::new();
    let mut outputs: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut started = BTreeSet::new();
    let mut times: BTreeMap<String, [u64; 2]> = BTreeMap::new();
    let mut logs = Vec::new();
    let mut shards = BTreeMap::new();
    let mut summaries = BTreeMap::new();
    let mut kept = BTreeMap::new();
    let (mut attempts, mut exit, mut running) = (0, Some(0), 0);
    let before = ctx.previous.as_ref().and_then(|p| {
        p.entry
            .workflows
            .iter()
            .find(|w| w.file == file)
            .map(|w| (p, w))
    });

    // Jobs that passed in the result this run keeps start as done, the loop below runs the rest
    if let Some((prev, wf)) = before {
        for job in keep_jobs(&graph, &wf.jobs) {
            let Some(log) = keep_log(ctx, &prev.logs, file, &job) else {
                continue;
            };
            let time = wf.times.get(&job).copied().unwrap_or_default();
            say(&kept_line(ctx, file, &job));
            job_done(ctx, file, &job, "success", time);
            logs.push(log);
            times.insert(job.clone(), time);

            if let Some(summary) = wf.summaries.get(&job) {
                summaries.insert(job.clone(), summary.clone());
            }

            if let Some(info) = wf.shards.get(&job) {
                shards.insert(job.clone(), info.clone());
            }

            if let Some(tree) = kept_from(ctx) {
                kept.insert(job.clone(), tree.to_string());
            }

            states.insert(job.clone(), "success".to_string());
            started.insert(job);
        }
    }

    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<Msg>();

        loop {
            let mut changed = true;

            while changed {
                changed = false;

                for (job, needs) in &graph {
                    if started.contains(job) || !needs.iter().all(|n| states.contains_key(n)) {
                        continue;
                    }

                    // A job whose need was skipped is skipped too, one whose need failed never runs
                    // An allowed failure counts as passed here, as on GitHub, so the jobs that need it still run
                    let unmet =
                        |n: &&String| states[*n] != "success" && !states[*n].ends_with(ALLOWED);

                    if let Some(n) = needs.iter().find(unmet) {
                        // GitHub runs a job whose `if:` checks status even when its need was skipped
                        let state = if states[n] == "skipped" && runs_anyway(job) {
                            NEED_SKIPPED
                        } else if states[n] == "skipped" {
                            "skipped"
                        } else {
                            NOT_RUN
                        };
                        started.insert(job.clone());
                        states.insert(job.clone(), state.into());
                        changed = true;
                        continue;
                    }

                    started.insert(job.clone());
                    times.insert(job.clone(), [unix_now(); 2]);

                    let needed: BTreeMap<String, BTreeMap<String, String>> = needs
                        .iter()
                        .map(|n| (n.clone(), outputs.get(n).cloned().unwrap_or_default()))
                        .collect();
                    let (tx, artifacts) = (tx.clone(), &artifacts);
                    running += 1;
                    s.spawn(move || {
                        let r = run_job(ctx, file, job, &needed, deadline, artifacts, &tx);
                        let _ = tx.send(Msg::Done(job.clone(), r));
                    });
                }
            }

            if running == 0 {
                break;
            }

            match rx.recv() {
                Ok(Msg::Say(m)) => say(&m),
                Ok(Msg::Done(job, result)) => {
                    running -= 1;

                    if let Some(time) = times.get_mut(&job) {
                        time[1] = unix_now();
                    }

                    let state = match result {
                        Ok(run) => {
                            let marks = workflow::read_marks(&run.log, ctx.nonce);
                            let act = if !run.unsupported.is_empty() {
                                ActRun::NoImage
                            } else if run.skipped {
                                ActRun::Skipped
                            } else {
                                ActRun::Ran
                            };
                            let on_error = if allowed.contains(&job) {
                                OnError::Continue
                            } else {
                                OnError::Fail
                            };
                            let state = job_state(&job, run.exit, act, &marks, on_error);
                            let state = act_skip(state, &job, &skip_on_prs);
                            let allow = state.ends_with(ALLOWED);

                            if exit == Some(0) && run.exit != Some(0) && !allow {
                                exit = run.exit;
                            }

                            attempts = attempts.max(run.attempts);
                            outputs.insert(job.clone(), run.outputs);

                            if let Some(info) = run.shard {
                                shards.insert(job.clone(), info);
                            }

                            logs.push(run.log);

                            if !run.summary.is_empty() {
                                summaries.insert(job.clone(), run.summary);
                            }

                            state
                        }
                        Err(e) => {
                            exit = exit.and(Some(1));
                            // The state goes into the check, which may be public, so the error with
                            // its host paths stays on this machine
                            say(&format!("{file}/{job}: {e}"));
                            error_state(&e).to_string()
                        }
                    };
                    say(&ended(
                        file,
                        &job,
                        &state,
                        times.get(&job).copied().unwrap_or_default(),
                    ));
                    job_done(
                        ctx,
                        file,
                        &job,
                        &state,
                        times.get(&job).copied().unwrap_or_default(),
                    );
                    states.insert(job, state);
                }
                Err(_) => break,
            }
        }
    });

    for (job, _) in &graph {
        states
            .entry(job.clone())
            .or_insert_with(|| "not run, its needs form a cycle".into());
    }

    let log = logs.concat();
    fs::write(ctx.logs.join(format!("{file}.log")), &log).map_err(|e| e.to_string())?;
    let passed = workflow_passed(&states);
    say(&format!(
        "{file}: {}",
        if passed { "passed" } else { "failed" }
    ));

    Ok(WorkflowResult {
        file: file.to_string(),
        exit,
        attempts,
        log_sha256: cache::hex(&Sha256::digest(log.as_bytes())),
        jobs: states,
        passed,
        shards,
        kept,
        times,
        summaries,
    })
}

/// What act said of one job's run
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActRun {
    Ran,
    /// Its `if:` skipped the job
    Skipped,
    /// No image for its runs-on label
    NoImage,
}

/// Whether a job's failure fails its workflow, or `continue-on-error` allows it
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnError {
    Fail,
    Continue,
}

/// Turns the act exit code of one job's run and the markers in its log into the job's state
fn job_state(
    job: &str,
    exit: Option<i32>,
    act: ActRun,
    marks: &BTreeMap<String, workflow::JobMarks>,
    on_error: OnError,
) -> String {
    let state = if act == ActRun::NoImage {
        "no image for its runs-on label on this host"
    } else if exit.is_none() {
        "timed out"
    } else if exit == Some(0) && marks.is_empty() && act == ActRun::Skipped {
        "skipped"
    } else if exit == Some(0) && marks.is_empty() {
        // act ran none of it and never said its `if:` skipped it
        "failure, act ran nothing"
    } else if exit == Some(0)
        && marks.contains_key(job)
        && marks.values().all(super::workflow::JobMarks::passed)
    {
        "success"
    } else if marks.get(job).is_some_and(|m| m.ended < m.started) {
        "killed"
    } else {
        "failure"
    };

    // A run that never finished, or hit shipcheck's deadline, is not the job failing
    if on_error == OnError::Continue && state == "failure" {
        format!("{state}{ALLOWED}")
    } else {
        state.to_string()
    }
}

/// act runs a PR's checks as `workflow_dispatch`, so its skip of a job, or of a matrix leg of one,
/// stands only when the job's `if:` is false on every PR, as GitHub would skip it too
fn act_skip(state: String, job: &str, skip_on_prs: &BTreeSet<String>) -> String {
    if state == "skipped" && !skip_on_prs.iter().any(|j| names_job(job, j)) {
        IF_SKIP.into()
    } else {
        state
    }
}

const IF_SKIP: &str =
    "failure, act skipped it by its `if:`, which may hold on a PR, and only GitHub can tell";

const NEED_SKIPPED: &str =
    "failure, a job it needs was skipped, but its `if:` may run it anyway, as GitHub would";

/// Each job's `if:` without init's gate, `false` for the YAML boolean
fn job_ifs(source: &str) -> BTreeMap<String, String> {
    let Ok(root) = workflow::load(source) else {
        return BTreeMap::new();
    };
    let Some(jobs) = root
        .get(&yaml_rust2::Yaml::String("jobs".into()))
        .and_then(|j| j.as_hash())
    else {
        return BTreeMap::new();
    };

    jobs.iter()
        .filter_map(|(id, job)| {
            let cond = match &job["if"] {
                yaml_rust2::Yaml::Boolean(b) => b.to_string(),
                yaml_rust2::Yaml::String(c) => workflow::ungate(c).unwrap_or(c.clone()),
                _ => return None,
            };
            Some((id.as_str()?.to_string(), cond))
        })
        .collect()
}

/// The jobs GitHub skips on every PR by their own `if:`
fn skipped_on_every_pr(ifs: &BTreeMap<String, String>) -> BTreeSet<String> {
    ifs.iter()
        .filter(|(_, c)| false_on_every_pr(c))
        .map(|(j, _)| j.clone())
        .collect()
}

/**
 * A condition that and-s a term false on every PR: a push event, one branch or a tag. An `||` or a
 * `!` other than `!=` could make it hold, so a condition with one never counts
 **/
/// A workflow passes when every job passed, was skipped or is an allowed failure, and one passed
/// or was skipped, as act said its `if:` skipped it, so GitHub counts it as passed
fn workflow_passed(states: &BTreeMap<String, String>) -> bool {
    states
        .values()
        .all(|s| s == "success" || s == "skipped" || s.ends_with(ALLOWED))
        && states.values().any(|s| s == "success" || s == "skipped")
}

/// A job shipcheck could not run gets why at the end of its log, so it has a tail like any failure
fn run_job(
    ctx: &Ctx<'_>,
    file: &str,
    job: &str,
    needs: &BTreeMap<String, BTreeMap<String, String>>,
    deadline: Instant,
    artifacts: &Path,
    tx: &mpsc::Sender<Msg>,
) -> Result<JobRun, String> {
    try_job(ctx, file, job, needs, deadline, artifacts, tx).inspect_err(|e| {
        let log = ctx.logs.join(file).join(format!("{job}.log"));
        note_error(&log, e);
    })
}

fn note_error(log: &Path, error: &str) {
    let _ = log.parent().map(fs::create_dir_all);
    let file = fs::OpenOptions::new().create(true).append(true).open(log);
    let _ = file.and_then(|mut f| writeln!(f, "shipcheck: {error}"));
}

/// Runs one job in its own clone at a fixed path per slot and job, and a failure again in a new one
#[expect(
    clippy::too_many_lines,
    reason = "one job attempt, from its clone to its state, read top to bottom"
)]
fn try_job(
    ctx: &Ctx<'_>,
    file: &str,
    job: &str,
    needs: &BTreeMap<String, BTreeMap<String, String>>,
    deadline: Instant,
    artifacts: &Path,
    tx: &mpsc::Sender<Msg>,
) -> Result<JobRun, String> {
    // A job of any workflow in this run waits here until fewer than `max_parallel` run
    let waited = Instant::now();
    let _slot = ctx.slots.take();
    let _going = ctx.note.job(format!("{file}/{job}"));
    timing(file, job, "slot wait", waited);
    let dir = ctx.slot.join(file).join(job);
    let template = ctx.templates.join(file).join(job);
    let log_path = ctx.logs.join(file).join(format!("{job}.log"));
    let say = |m: String| {
        let _ = tx.send(Msg::Say(m));
    };
    let mut attempts = 0;
    // Attempts before the unsharded run, which gets the full retries of its own
    let mut before = 0;
    let mut shard = ctx.plan.config.shard_tests.enabled();
    let mut info = None;
    let mut infra_left = INFRA_RETRIES;

    loop {
        let timeout = deadline.saturating_duration_since(Instant::now());

        // An attempt that never started is no attempt
        if timeout.is_zero() {
            return Ok(JobRun {
                exit: None,
                skipped: false,
                attempts,
                outputs: BTreeMap::new(),
                unsupported: Vec::new(),
                log: String::from_utf8_lossy(&fs::read(&log_path).unwrap_or_default()).into(),
                shard: info,
                summary: String::new(),
            });
        }

        attempts += 1;
        let attempt_at = Instant::now();
        say(format!("{file}/{job}: running, attempt {attempts}"));
        {
            let at = Instant::now();
            let _git = git_lock(ctx.repo_id)?;
            timing(file, job, "git lock wait", at);
            let at = Instant::now();
            checkout(ctx, runner_of(file), &template, &dir, &say)?;
            timing(file, job, "git-cow clone", at);
        }

        let at = Instant::now();
        let link = standalone_git(&dir, &ctx.plan.sha)?;
        timing(file, job, "standalone_git", at);
        // The commit the remote reports, never `--base` or a local ref such as origin/main, which a
        // caller can point at a branch. Only host checks keep a cache per base
        let on_base = file == HOST
            && ctx
                .base_tree
                .get_or_init(|| {
                    let sha = remote_base_sha(&ctx.plan.repo)?;
                    git(
                        &ctx.plan.repo,
                        &["rev-parse", "--verify", "-q", &format!("{sha}^{{tree}}")],
                    )
                    .ok()
                })
                .as_ref()
                .is_some_and(|t| *t == ctx.plan.tree);
        let workflow = dir.with_extension("workflows").join(file);
        let request = RunRequest {
            run_dir: &dir,
            file,
            workflow: &workflow,
            job,
            needs,
            artifacts,
            repo_id: ctx.repo_id,
            timeout,
            shard,
            on_base,
            slot_lock: Some(ctx.slot_lock),
        };
        let host = file == HOST;
        let runner = ctx.runners.for_file(file)?;
        let at = Instant::now();
        let written = if host {
            Ok(())
        } else {
            write_run_files(ctx, &dir, file, &workflow)
        };
        timing(file, job, "write_run_files", at);

        // Host checks start no container, so there is no engine view to wait out
        if !host {
            let at = Instant::now();
            wait_for_engine(&ended_stamp(ctx.slot, &dir));
            timing(file, job, "engine view wait", at);
        }

        let at = Instant::now();
        let outcome = written.and_then(|()| runner.run(&request, &log_path));
        timing(file, job, "runner", at);

        // A container left running, after a timeout or otherwise, could rewrite the checkout's `.git`
        // while shipcheck warms from it, so none may outlive the job
        let at = Instant::now();
        runner.reclaim(&dir);
        timing(file, job, "reclaim", at);
        mark_ended(&ended_stamp(ctx.slot, &dir));
        // Only now, with no container left that could swap them, are the job's reports read
        let sharded = outcome
            .as_ref()
            .ok()
            .and_then(|o| o.shards.map(|n| (n, o.exit)));
        let accepted = sharded.map(|(n, exit)| match exit {
            Some(0) => shard::check(&shard::read_reports(&dir, n), n).map(|items| (n, items)),
            _ => Err(Rejected::JobFailed),
        });

        let at = Instant::now();
        finish_job(&dir, &template, link.as_deref(), &say);
        timing(file, job, "warm and empty", at);
        let outcome = outcome?;

        // Any failure of a sharded attempt runs the job again whole, whose result is the result
        match accepted {
            Some(Ok((n, items))) => {
                info = Some(ShardRun {
                    shards: Some(n),
                    items: Some(items),
                    fallback: None,
                });
            }
            Some(Err(why)) => {
                say(format!(
                    "{file}/{job}: sharded run not accepted ({why}), running it again unsharded"
                ));
                info = Some(ShardRun {
                    shards: None,
                    items: None,
                    fallback: Some(why.to_string()),
                });
                let kept = keep_attempt_log(&log_path, attempts)?;
                say(format!(
                    "{file}/{job}: the sharded attempt's log is {}",
                    kept.display()
                ));
                (shard, before) = (false, attempts);
                continue;
            }
            None => {}
        }

        let failed = outcome.exit.is_some_and(|e| e != 0) && outcome.unsupported_jobs.is_empty();
        let mut invalid = false;

        // A failure of Docker's, such as a pull timeout, gets attempts of its own beside `retries`
        if failed && !host {
            let log = fs::read(&log_path).map_err(|e| e.to_string())?;
            let log = String::from_utf8_lossy(&log);
            invalid = act::invalid_workflow(&log);

            if !invalid && infra_left > 0 && act::never_started(&log) {
                (infra_left, before) = (infra_left - 1, before + 1);
            }
        }

        if outcome.exit == Some(0) || invalid || attempts - before > ctx.plan.config.retries {
            let log = fs::read(&log_path).map_err(|e| e.to_string())?;
            return Ok(JobRun {
                exit: outcome.exit,
                skipped: outcome.skipped,
                attempts,
                outputs: outcome.outputs,
                unsupported: outcome.unsupported_jobs,
                log: String::from_utf8_lossy(&log).into(),
                shard: info,
                summary: outcome.summary,
            });
        }

        let kept = keep_attempt_log(&log_path, attempts)?;
        say(format!(
            "{file}/{job}: attempt {attempts} of {} failed after {}, its log is {}",
            before + ctx.plan.config.retries + 1,
            clock(attempt_at.elapsed().as_secs()),
            kept.display()
        ));
    }
}

/// Attempts a job gets when Docker, not the job, failed it before its first step
const INFRA_RETRIES: u32 = 2;

/// How long after a job's containers end the next one at that path waits, measured on Docker Desktop 4.x
const ENGINE_VIEW: Duration = Duration::from_secs(2);

/// When the last containers of a job checkout ended, kept beside the slot, which a new run deletes
fn ended_stamp(slot: &Path, dir: &Path) -> PathBuf {
    let rel = dir
        .strip_prefix(slot)
        .ok()
        .filter(|r| !r.as_os_str().is_empty());
    slot.with_extension("ended")
        .join(rel.unwrap_or(Path::new("slot")))
}

fn mark_ended(stamp: &Path) {
    if let Some(parent) = stamp.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let _ = fs::write(stamp, b"");
}

/**
 * Docker Desktop shows a path as the last container saw it for about a second, so a job, a retry or
 * the next run's, starts no sooner than `ENGINE_VIEW` after the last containers at its path ended
 **/
fn wait_for_engine(stamp: &Path) {
    std::thread::sleep(engine_wait(stamp, SystemTime::now()));
}

fn engine_wait(stamp: &Path, now: SystemTime) -> Duration {
    let Ok(ended) = fs::metadata(stamp).and_then(|m| m.modified()) else {
        return Duration::ZERO;
    };

    // A stamp from the future means the clock moved, so wait the whole gap
    now.duration_since(ended)
        .map_or(ENGINE_VIEW, |since| ENGINE_VIEW.saturating_sub(since))
}

/// Moves a failed attempt's log aside, so the next attempt's `<job>.log` cannot overwrite it
fn keep_attempt_log(log_path: &Path, attempt: u32) -> Result<PathBuf, String> {
    let stem = log_path.file_stem().unwrap_or_default().to_string_lossy();
    let kept = log_path.with_file_name(format!("{stem}.attempt-{attempt}.log"));
    fs::rename(log_path, &kept).map_err(|e| e.to_string())?;
    Ok(kept)
}

/**
 * Warms the template from a finished job's checkout, then empties it. Warming runs git in the
 * checkout, where the job's own `.git` could name commands such as `core.fsmonitor` that git starts
 * on the host. So the worktree's `.git` file, which points into the host repository, goes back first
 **/
fn finish_job(dir: &Path, template: &Path, link: Option<&str>, say: &dyn Fn(String)) {
    let restored = link.map(|link| {
        let git = dir.join(".git");

        if fs::symlink_metadata(&git).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(&git)?;
        } else if fs::symlink_metadata(&git).is_ok() {
            fs::remove_file(&git)?;
        }

        fs::write(&git, link)
    });

    match restored {
        Some(Ok(())) => {
            if let Err(e) = git_cow::warm(dir, template) {
                say(format!("could not warm the template: {e}"));
            }
        }
        Some(Err(e)) => say(format!("could not warm the template: {e}")),
        None => {}
    }

    // git still lists the emptied worktree, the run prunes it once all its jobs are done
    if let Err(e) = empty_dir(dir) {
        say(format!("could not empty {}: {e}", dir.display()));
    }
}

fn checkout(
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

/**
 * The clone keeps the template's modification times, and git rewrites only the files that differ
 * from the template's base. The template's build output can come from another tree and another
 * slot, so cargo or make would trust it for a file this tree has unchanged. Every tracked file is
 * made newer than any build output, so each run builds its own tree in its own slot
 **/
fn touch_tracked(dir: &Path) -> Result<(), String> {
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

/**
 * Proves the job's files are the tested tree: every tracked file on disk is hashed again into a new
 * index, which has no stat cache to skip a file with, and the tree written from it must be the
 * plan's. A file edited, deleted, relinked or with another mode changes that tree. Ignored and
 * untracked files, such as warm build output, are not part of a tree, so they never count
 **/
fn verify_checkout(dir: &Path, tree: &str) -> Result<(), String> {
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

/**
 * Docker Desktop's file sharing can keep a bind mount on a directory that was deleted and made again
 * at the same path, so a job saw an empty checkout and could not exec. A job's checkout directory is
 * made once and only emptied between runs
 **/
fn empty_dir(dir: &Path) -> std::io::Result<()> {
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

/**
 * A job checkout is a git worktree, whose `.git` file points into the repository's own `.git` on the
 * host, which the job cannot see and must not read: it holds unpushed branches, the stash and remote
 * credentials. So the checkout gets a new repository with only what the tested commit and the tags
 * reach, fetched from the worktree itself, whose `HEAD` is that commit. A job may change it freely.
 * Returns the worktree's `.git` file, which [`finish_job`] puts back
 **/
fn standalone_git(dir: &Path, sha: &str) -> Result<Option<String>, String> {
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
fn job_git(dir: &Path, gitdir: &Path, args: &[&str]) -> Result<String, String> {
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
fn remove_planted_links(dir: &Path) -> Result<(), String> {
    for path in [dir.join(".github"), dir.join(WORKFLOWS), dir.join(CONFIG)] {
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }

    Ok(())
}

/// Puts the workflows and config from the base into the checkout, so the commit cannot change what runs
fn write_run_files(ctx: &Ctx<'_>, dir: &Path, file: &str, workflow: &Path) -> Result<(), String> {
    let wf_dir = dir.join(WORKFLOWS);
    remove_planted_links(dir)?;
    fs::create_dir_all(&wf_dir).map_err(|e| e.to_string())?;

    // Only the workflow files themselves, unchanged, so a `git diff` in a job sees nothing and the
    // scripts and data next to them stay
    for (path, body) in &ctx.plan.workflows {
        let target = wf_dir.join(file_name(path));

        if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(&target).map_err(|e| e.to_string())?;
        }

        fs::write(&target, body).map_err(|e| e.to_string())?;
    }

    let parent = workflow.parent().ok_or("workflow path has no parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    fs::write(workflow, &ctx.bodies[file]).map_err(|e| e.to_string())
}

/// Held around `git worktree` changes for one repository, across processes, since a prune while
/// another job adds its worktree deletes that worktree's half-made admin files
fn git_lock(repo_id: &str) -> Result<fs::File, String> {
    git_lock_in(&home(), repo_id)
}

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

const KEEP_RESULTS: Duration = Duration::from_hours(336);
const KEEP_LOGS: Duration = Duration::from_hours(2160);

/// What `clean` removes: one repository's state, or every repository's
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    Repo(&'a Path),
    All,
}

/**
 * Removes what shipcheck keeps for one repository: its Docker volumes, warm template and run slots,
 * or for every repository with `All`, which also clears results, logs and records. Everything but
 * the records rebuilds on the next run, which is only slower. Refuses while a run holds a slot
 **/
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
fn glob_locks(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
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

#[cfg(test)]
mod tests;
