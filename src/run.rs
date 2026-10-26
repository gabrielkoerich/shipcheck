use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::{self, KeyInputs};
use crate::config::{RepoConfig, RunnerKind};
use crate::runner::host::{self as host_runner, HOST};
use crate::runner::shard::{self, Rejected, ShardRun};
use crate::runner::{RunRequest, Runner, Runners};
use crate::workflow;

pub(crate) const WORKFLOWS: &str = ".github/workflows";
const CONFIG: &str = "shipcheck.toml";

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

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Everything about one commit that the cache key and a run need, read from git
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
    /// The base has no shipcheck.toml, so the plan runs the defaults, and its result is never posted
    pub defaults: bool,
    /// The runners' identity in the key: act's version, then the host checks' tool versions
    pub tools: String,
}

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

/// The `GIT_CONFIG_*` variables for SAFE_GIT, after the `existing` entries the user already set
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

pub fn home() -> PathBuf {
    // A relative path would name another folder from every clone and git command that changes directory
    let absolute =
        |dir: std::ffi::OsString| std::path::absolute(&dir).unwrap_or_else(|_| dir.into());

    // An empty value is unset, as XDG says, not the current directory
    let set = |name| std::env::var_os(name).filter(|v| !v.is_empty());

    if let Some(dir) = set("SHIPCHECK_HOME") {
        return absolute(dir);
    }

    if let Some(state) = set("XDG_STATE_HOME") {
        return absolute(state).join("shipcheck");
    }

    env_home().join(".local/state/shipcheck")
}

fn env_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Where the home was on macOS before it moved to `~/.local/state`
pub fn legacy_home() -> PathBuf {
    env_home().join("Library/Application Support/shipcheck")
}

#[derive(Debug, PartialEq)]
enum Migration {
    Nothing,
    Moved,
    Both,
}

/// Renames `old` to `new` when only `old` exists. A rename, since the home holds clones a copy
/// would inflate
fn move_home(old: &Path, new: &Path) -> Result<Migration, String> {
    use std::os::unix::fs::PermissionsExt;

    match (old.is_dir(), new.exists()) {
        (false, _) => return Ok(Migration::Nothing),
        (true, true) => return Ok(Migration::Both),
        (true, false) => {}
    }

    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }

    fs::rename(old, new)
        .and_then(|_| fs::set_permissions(new, fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("moving {} to {}: {e}", old.display(), new.display()))?;
    Ok(Migration::Moved)
}

/// Moves the macOS home out of `~/Library` on first use. Leaves both alone when both exist, which
/// `shipcheck doctor` reports
pub fn migrate_home() {
    if !cfg!(target_os = "macos") || std::env::var_os("SHIPCHECK_HOME").is_some() {
        return;
    }

    let (old, new) = (legacy_home(), home());

    match move_home(&old, &new) {
        Ok(Migration::Moved) => {
            eprintln!("shipcheck: moved {} to {}", old.display(), new.display())
        }
        Ok(_) => {}
        Err(e) => eprintln!("shipcheck: {e}"),
    }
}

/// Clones, run slots and tool caches, which every run can rebuild and which hold hundreds of GB
#[cfg(target_os = "macos")]
const REBUILT: [&str; 3] = ["templates", "runs", "hostcache"];

/**
 * Keeps Time Machine off what a run can rebuild, so a backup holds the results and logs and not
 * clones of every repository. `tmutil addexclusion` sets a flag on the folder itself, needs no root
 * and follows the folder, so it runs only when the flag is missing
 **/
#[cfg(not(target_os = "macos"))]
fn exclude_from_backup(_: &Path) {}

#[cfg(target_os = "macos")]
fn exclude_from_backup(home: &Path) {
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
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // Safe: a size query with a null buffer reads nothing
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

/// The home of a user who set nothing, from the passwd entry, since an agent can set any variable
pub fn default_home() -> PathBuf {
    user_dir().join(".local/state/shipcheck")
}

/// Whether the results live somewhere else than the default, where an agent may have written them
pub fn home_overridden() -> bool {
    home() != default_home()
}

/// The user's home directory from the passwd entry, empty when there is none, so a check against
/// it fails closed
pub fn user_dir() -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    let mut buf = vec![0 as libc::c_char; 16384];
    // Safe: getpwuid_r writes only into pwd and buf, and pw_dir points into buf on success
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut found = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        );

        if rc != 0 || found.is_null() || pwd.pw_dir.is_null() {
            return PathBuf::new();
        }

        let dir = std::ffi::CStr::from_ptr(pwd.pw_dir).to_bytes();
        PathBuf::from(std::ffi::OsStr::from_bytes(dir))
    }
}

fn git_raw(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
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

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&git_raw(repo, args)?)
        .trim()
        .to_string())
}

/// Reads the commit, and the config and workflows from `base`, never from the commit or the working copy
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
        if path.ends_with(".yml") || path.ends_with(".yaml") {
            workflows.insert(
                path.to_string(),
                git_raw(&repo, &["show", &format!("{base}:{path}")])?,
            );
        }
    }

    // Without the owner's file, run what GitHub runs on a PR, so an agent can start alone. Such a
    // result is never posted as the check, which stays tied to the owner's rules
    let (config_text, defaults) = match git_raw(&repo, &["show", &format!("{base}:{CONFIG}")]) {
        Ok(text) => (text, false),
        Err(_) => {
            let on_prs: Vec<String> = workflows
                .iter()
                .filter(|(_, body)| workflow::runs_for_branches(&String::from_utf8_lossy(body)))
                .map(|(path, _)| file_name(path))
                .collect();

            if on_prs.is_empty() {
                return Err(format!(
                    "{base_ref} has no {CONFIG} and no workflow that runs for branches. Run `shipcheck init`"
                ));
            }

            let text = format!(
                "# No {CONFIG} on {base_ref}: every workflow that runs for branches, on pull_request or push\n{}",
                crate::setup::config_for(&on_prs)
            );
            (text.into_bytes(), true)
        }
    };
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

    let tools = runners.identity()?;
    let key = cache::key(&KeyInputs {
        tree: &tree,
        config: &config_text,
        workflows: &as_run,
        runner_kind: runners.kind(),
        checks: &config.host.checks,
        runner_version: &tools,
        shipcheck_version: env!("CARGO_PKG_VERSION"),
        platforms: &crate::runner::act::platforms(&config.act),
    });

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
        defaults,
        tools,
    })
}

/**
 * The working copy, or with `staged` the index, as a commit no ref names, with HEAD as parent, like
 * `git stash create`. The working copy is HEAD's tree with every edit and every file that is not
 * gitignored. It is built in a copy of the index, so the user's index, branches and stash stay as
 * they are. None when the tree is HEAD's
 **/
pub fn snapshot(cwd: &Path, staged: bool) -> Result<Option<String>, String> {
    let repo = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);
    let head = git(&repo, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let (index, temp) = index_paths(&repo)?;
    let tree = snapshot_tree(&repo, &index, &temp, staged);
    let _ = fs::remove_file(&temp);
    let tree = tree?;

    if tree == git(&repo, &["rev-parse", "HEAD^{tree}"])? {
        return Ok(None);
    }

    let what = if staged { "index" } else { "working copy" };
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

fn snapshot_tree(repo: &Path, index: &Path, temp: &Path, staged: bool) -> Result<String, String> {
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

    if !staged {
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
pub fn remote_base(repo: &Path) -> Option<String> {
    static ASKED: std::sync::Mutex<BTreeMap<(PathBuf, String), Option<String>>> =
        std::sync::Mutex::new(BTreeMap::new());
    let url = git(repo, &["remote", "get-url", "origin"]).unwrap_or_default();
    let mut asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
    asked
        .entry((repo.to_path_buf(), url))
        .or_insert_with(|| remote_default(repo).map(|b| format!("origin/{b}")))
        .clone()
}

/// `git ls-remote --symref origin HEAD`'s branch, None when the remote does not answer in 5s
fn remote_default(repo: &Path) -> Option<String> {
    use std::io::Read;

    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
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
    out.lines().find_map(|l| {
        let (branch, name) = l.strip_prefix("ref: refs/heads/")?.split_once('\t')?;
        (name == "HEAD" && !branch.is_empty()).then(|| branch.to_string())
    })
}

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
pub fn commit(
    cwd: &Path,
    tested: &Tested,
    git_args: &[String],
    allow_failed: bool,
) -> Result<String, String> {
    let repo = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);

    if tested.partial {
        return Err(
            "the last run was partial, with --job or --workflow, which is not the check: run `shipcheck run`, then `shipcheck commit`".into(),
        );
    }

    if !tested.passed && !allow_failed {
        return Err(
            "the run failed, fix it and run `shipcheck run` again, or pass --allow-failed".into(),
        );
    }

    let (index, temp) = index_paths(&repo)?;
    let committed = snapshot_tree(&repo, &index, &temp, tested.staged).and_then(|tree| {
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

        // From the caller's directory, so a relative `-F` file is found
        let status = git_commit
            .current_dir(cwd)
            .arg("commit")
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
                found.push(format!("{shown} is added, it can change what checks run"))
            }
            (Some(_), None) => {
                found.push(format!("{shown} is removed, it can change what checks run"))
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
) -> Result<crate::check::Jobs, String> {
    let mut graphs = BTreeMap::new();

    for file in config.act_files() {
        let body = source(&file).ok_or_else(|| format!("{file} is not on the base"))?;
        graphs.insert(file, workflow::job_needs(&body)?);
    }

    let mut jobs = act_jobs(config, &graphs)?;

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
fn act_jobs(config: &RepoConfig, graphs: &Graphs) -> Result<crate::check::Jobs, String> {
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
pub fn jobs(plan: &Plan) -> Result<crate::check::Jobs, String> {
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
    crate::record::runs(home, repo, usize::MAX)
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
    entry.ci_changes = plan.ci_changes.clone();
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
    let logs = entry.workflows.iter().all(|wf| {
        fs::read(log_dir(&entry.key).join(format!("{}.log", wf.file)))
            .is_ok_and(|log| cache::hex(&Sha256::digest(&log)) == wf.log_sha256)
    });
    entry.tree == plan.tree && on_base && logs
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

pub fn log_dir(key: &str) -> PathBuf {
    home().join("logs").join(key)
}

/// Job output with its control characters escaped, so it cannot drive the terminal it is printed to
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\t' => c.to_string(),
            c if c.is_control() => c.escape_debug().to_string(),
            c => c.to_string(),
        })
        .collect()
}

pub fn secrets_file(repo_id: &str) -> PathBuf {
    home().join("secrets").join(format!("{repo_id}.env"))
}

/// The values in a repository's secrets file, longest first, to redact from anything posted. Each
/// line of a value with `\n` escapes is a value too, since a log prints it line by line
pub fn secret_values(repo_id: &str) -> Result<Vec<String>, String> {
    let text = crate::secret::read(&secrets_file(repo_id))?.unwrap_or_default();
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

/// Whether a run of this key is going right now on this machine, without waiting for it
pub fn running(key: &str) -> bool {
    let path = home().join("locks").join(format!("{key}.lock"));

    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .is_ok_and(|lock| lock.try_lock().is_err())
}

fn max_runs() -> usize {
    std::env::var("SHIPCHECK_MAX_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() / 4))
        .max(1)
}

/// Waits for one of the machine's run slots, which cap how many act runs go at once
fn claim_runner(say: &mut dyn FnMut(&str)) -> Result<fs::File, String> {
    let dir = home().join("runners");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut told = false;

    loop {
        for n in 0..max_runs() {
            let lock =
                fs::File::create(dir.join(format!("{n}.lock"))).map_err(|e| e.to_string())?;

            if lock.try_lock().is_ok() {
                return Ok(lock);
            }
        }

        if !told {
            say("waiting for a free run slot");
            told = true;
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Runs every required workflow of the plan, each job in its own copy-on-write clone, and caches
/// the result
pub fn run(plan: &Plan, say: &mut dyn FnMut(&str)) -> Result<Entry, String> {
    let entry = execute(plan, None, say)?;

    // Docker or the sandbox failed before the code ran, or a clone was wrong, so there is no verdict to keep
    if entry.could_not_run() || entry.wrong_tree() {
        return Ok(entry);
    }

    let cache_dir = home().join("cache");
    fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&entry).map_err(|e| e.to_string())?;
    fs::write(cache_dir.join(format!("{}.json", plan.key)), json).map_err(|e| e.to_string())?;
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
    execute(plan, Some((jobs, workflows)), say)
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
    checks: &BTreeMap<String, String>,
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

pub fn partial_log_dir(key: &str) -> PathBuf {
    home().join("logs").join(format!("{key}.partial"))
}

type Selection<'a> = Option<(&'a [String], &'a [String])>;

fn execute(plan: &Plan, only: Selection, say: &mut dyn FnMut(&str)) -> Result<Entry, String> {
    let _runner = claim_runner(say)?;
    let home = home();
    // Results, logs, secrets and the App key live here, other users of the machine read none of it
    let private = <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700);
    fs::set_permissions(&home, private).map_err(|e| format!("{}: {e}", home.display()))?;
    exclude_from_backup(&home);
    let id = repo_id(&plan.repo)?;
    let templates = home.join("templates").join(&id);
    let slots = home.join("runs").join(&id);
    fs::create_dir_all(&slots).map_err(|e| e.to_string())?;
    let (slot, _slot) = claim_slot(&slots)?;
    let runners = Runners::new(&plan.config, &plan.repo);

    for runner in runners.all() {
        runner.preflight()?;
    }

    prune(&home, SystemTime::now());
    crate::templates::prune(&home, SystemTime::now(), &slot.with_extension("lock"));
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
    let slots = Slots::new(
        plan.config
            .max_parallel(crate::runner::act::engine_resources()),
    );
    let ctx = Ctx {
        slot_lock: std::os::fd::AsRawFd::as_raw_fd(&_slot),
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
        only: only.as_ref(),
        base_tree: std::sync::OnceLock::new(),
    };

    let results = run_all(&ctx, say)?;

    {
        let _git = git_lock(&id)?;
        let _ = git(&plan.repo, &["worktree", "prune"]);
    }

    for line in runners.all().flat_map(|r| r.prune()) {
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
        defaults: false,
        tools: String::new(),
    };
    let entry = execute(&plan, None, say);
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
fn run_all(ctx: &Ctx, say: &mut dyn FnMut(&str)) -> Result<Vec<WorkflowResult>, String> {
    let plan = ctx.plan;
    let host = ctx.runners.host.is_some() && ctx.only.is_none_or(|o| o.contains_key(HOST));
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
fn run_host(ctx: &Ctx, say: &mut dyn FnMut(&str)) -> Result<WorkflowResult, String> {
    let checks = &ctx.plan.config.host.checks;
    let (templates, checkouts) = (ctx.templates.join(HOST), ctx.slot.join(HOST));
    fs::create_dir_all(&templates).map_err(|e| e.to_string())?;
    fs::create_dir_all(&checkouts).map_err(|e| e.to_string())?;
    fs::create_dir_all(ctx.logs.join(HOST)).map_err(|e| e.to_string())?;
    git_cow::can_clone(&templates, &checkouts);
    let deadline = Instant::now() + Duration::from_secs(ctx.plan.config.timeout_minutes * 60);

    let runs: Vec<(String, Result<JobRun, String>, [u64; 2])> = std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<Msg>();
        let handles: Vec<_> = checks
            .keys()
            .filter(|name| {
                ctx.only
                    .is_none_or(|o| o.get(HOST).is_some_and(|picked| picked.contains(*name)))
            })
            .map(|name| {
                let tx = tx.clone();
                s.spawn(move || {
                    let (none, unused) = (BTreeMap::new(), ctx.slot.join(HOST));
                    let start = unix_now();
                    let run = run_job(ctx, HOST, name, &none, deadline, &unused, &tx);
                    (name.clone(), run, [start, unix_now()])
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

    let (mut states, mut times) = (BTreeMap::new(), BTreeMap::new());
    let (mut exit, mut attempts, mut log) = (Some(0), 0, String::new());
    let mut summaries = BTreeMap::new();

    for (name, run, time) in runs {
        times.insert(name.clone(), time);
        let state = match run {
            Ok(run) => {
                attempts = attempts.max(run.attempts);
                log.push_str(&run.log);

                if !run.summary.is_empty() {
                    summaries.insert(name.clone(), run.summary);
                }

                if exit == Some(0) && run.exit != Some(0) {
                    exit = run.exit;
                }

                match run.exit {
                    Some(0) => "success",
                    None => "timed out",
                    Some(_) => "failure",
                }
            }
            Err(e) => {
                exit = exit.and(Some(1));
                say(&format!("{HOST}/{name}: {e}"));
                error_state(&e)
            }
        };
        say(&format!("{HOST}/{name}: {state}"));
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
        shards: BTreeMap::new(),
        times,
        summaries,
    })
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
    let _ = git(&plan.repo, &["worktree", "prune"]);
}

/// Ends the state of a failed job with `continue-on-error: true`, which does not fail the workflow
pub const ALLOWED: &str = ", allowed by continue-on-error";

/// The job slots of one run, so all its workflows together start at most `max_parallel` jobs
struct Slots {
    free: std::sync::Mutex<usize>,
    freed: std::sync::Condvar,
}

impl Slots {
    fn new(count: usize) -> Self {
        Slots {
            free: std::sync::Mutex::new(count.max(1)),
            freed: std::sync::Condvar::new(),
        }
    }

    /// Waits for a free slot, which goes back when the returned guard drops
    fn take(&self) -> SlotGuard<'_> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());

        while *free == 0 {
            free = self.freed.wait(free).unwrap_or_else(|e| e.into_inner());
        }

        *free -= 1;
        SlotGuard(self)
    }
}

struct SlotGuard<'a>(&'a Slots);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.0.freed.notify_one();
    }
}

struct Ctx<'a> {
    /// The run slot's lock, which a job's reaper inherits
    slot_lock: std::os::fd::RawFd,
    slots: &'a Slots,
    plan: &'a Plan,
    runners: &'a Runners,
    /// The jobs act runs per workflow file, for every job of the run
    acts: &'a crate::check::Jobs,
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
fn run_workflow(
    ctx: &Ctx,
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
    let (mut attempts, mut exit, mut running) = (0, Some(0), 0);

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
                            let state = job_state(
                                &job,
                                run.exit,
                                run.skipped,
                                !run.unsupported.is_empty(),
                                &marks,
                                allowed.contains(&job),
                            );
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
                    say(&format!("{file}/{job}: {state}"));
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
        times,
        summaries,
    })
}

/// Turns the act exit code of one job's run and the markers in its log into the job's state
fn job_state(
    job: &str,
    exit: Option<i32>,
    skipped: bool,
    unsupported: bool,
    marks: &BTreeMap<String, workflow::JobMarks>,
    allowed: bool,
) -> String {
    let state = if unsupported {
        "no image for its runs-on label on this host"
    } else if exit.is_none() {
        "timed out"
    } else if exit == Some(0) && marks.is_empty() && skipped {
        "skipped"
    } else if exit == Some(0) && marks.is_empty() {
        // act ran none of it and never said its `if:` skipped it
        "failure, act ran nothing"
    } else if exit == Some(0) && marks.contains_key(job) && marks.values().all(|m| m.passed()) {
        "success"
    } else if marks.get(job).is_some_and(|m| m.ended < m.started) {
        "killed"
    } else {
        "failure"
    };

    // A run that never finished, or hit shipcheck's deadline, is not the job failing
    if allowed && state == "failure" {
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
pub(crate) fn false_on_every_pr(cond: &str) -> bool {
    let c = cond.trim();
    let c = c
        .strip_prefix("${{")
        .and_then(|c| c.strip_suffix("}}"))
        .unwrap_or(c)
        .trim();

    if c == "false" {
        return true;
    }

    !c.contains("||")
        && !c.replace("!=", "").contains('!')
        && [
            "github.event_name == 'push'",
            "github.ref == ",
            "github.ref_name == ",
            "startsWith(github.ref, 'refs/tags/",
        ]
        .iter()
        .any(|p| c.contains(p))
}

/// A workflow passes when every job passed, was skipped or is an allowed failure, and one passed
/// or was skipped, as act said its `if:` skipped it, so GitHub counts it as passed
fn workflow_passed(states: &BTreeMap<String, String>) -> bool {
    states
        .values()
        .all(|s| s == "success" || s == "skipped" || s.ends_with(ALLOWED))
        && states.values().any(|s| s == "success" || s == "skipped")
}

/// Runs one job in its own clone at a fixed path per slot and job, and a failure again in a new one
fn run_job(
    ctx: &Ctx,
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
        say(format!("{file}/{job}: running, attempt {attempts}"));
        {
            let at = Instant::now();
            let _git = git_lock(ctx.repo_id)?;
            timing(file, job, "git lock wait", at);
            let at = Instant::now();
            checkout(ctx, file == HOST, &template, &dir, &say)?;
            timing(file, job, "git-cow clone", at);
        }

        let at = Instant::now();
        let link = standalone_git(&dir, &ctx.plan.sha)?;
        timing(file, job, "standalone_git", at);
        // The default branch the remote reports, never `--base` or the local origin/HEAD, which a
        // caller can point at a branch. Only host checks keep a cache per base
        let on_base = file == HOST
            && ctx
                .base_tree
                .get_or_init(|| {
                    let base = remote_base(&ctx.plan.repo)?;
                    git(
                        &ctx.plan.repo,
                        &["rev-parse", "--verify", "-q", &format!("{base}^{{tree}}")],
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
        let outcome = written.and_then(|_| runner.run(&request, &log_path));
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

        // A failure of Docker's, such as a pull timeout, gets attempts of its own beside `retries`
        if failed && !host && infra_left > 0 {
            let log = fs::read(&log_path).map_err(|e| e.to_string())?;

            if crate::runner::act::never_started(&String::from_utf8_lossy(&log)) {
                (infra_left, before) = (infra_left - 1, before + 1);
            }
        }

        if outcome.exit == Some(0) || attempts - before > ctx.plan.config.retries {
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
            "{file}/{job}: attempt {attempts} failed, its log is {}",
            kept.display()
        ));
    }
}

/// Attempts a job gets when Docker, not the job, failed it before its first step
const INFRA_RETRIES: u32 = 2;

/// With `SHIPCHECK_TIMINGS` set, prints how long a phase of a job took, for measuring overhead
pub(crate) fn timing(file: &str, job: &str, phase: &str, since: Instant) {
    if std::env::var_os("SHIPCHECK_TIMINGS").is_some() {
        eprintln!(
            "timing {file}/{job} {phase} {:.3}",
            since.elapsed().as_secs_f64()
        );
    }
}

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
 * the next run's, starts no sooner than ENGINE_VIEW after the last containers at its path ended
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
    ctx: &Ctx,
    host: bool,
    template: &Path,
    dir: &Path,
    say: &dyn Fn(String),
) -> Result<(), String> {
    if dir.exists() {
        empty_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let _ = git(&ctx.plan.repo, &["worktree", "prune"]);
    }

    let parent = template.parent().ok_or("template has no parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    // Host checks build as the developer does, so git-cow warms their template from the
    // developer's checkout, with the repository's `cow.share` and `cow.warmInterval`
    let opts = if host {
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
    use std::os::unix::ffi::OsStrExt;

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
fn write_run_files(ctx: &Ctx, dir: &Path, file: &str, workflow: &Path) -> Result<(), String> {
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

pub(crate) fn git_lock_in(home: &Path, repo_id: &str) -> Result<fs::File, String> {
    let dir = home.join("locks").join("git");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = repo_id.replace('/', "%2F");
    let lock = fs::File::create(dir.join(format!("{name}.lock"))).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    Ok(lock)
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

const KEEP_RESULTS: Duration = Duration::from_secs(14 * 24 * 3600);
const KEEP_LOGS: Duration = Duration::from_secs(90 * 24 * 3600);

/**
 * Removes what shipcheck keeps for one repository: its Docker volumes, warm template and run slots,
 * or for every repository with `all`, which also clears results, logs and records. Everything but
 * the records rebuilds on the next run, which is only slower. Refuses while a run holds a slot
 **/
pub fn clean(repo: Option<&Path>, all: bool) -> Result<Vec<String>, String> {
    let home = home();
    let mut done = Vec::new();
    let id = match repo {
        Some(path) if !all => Some(repo_id(path)?),
        _ => None,
    };
    let runs = match &id {
        Some(id) => home.join("runs").join(id),
        None => home.join("runs"),
    };

    if let Ok(locks) = glob_locks(&runs) {
        for lock in locks {
            let file = fs::File::open(&lock).map_err(|e| e.to_string())?;

            if file.try_lock().is_err() {
                return Err("a run is in progress, clean once it finishes".into());
            }
        }
    }

    done.extend(crate::runner::act::Act::default().clean(id.as_deref()));
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

/// Every `<n>.lock` under `dir`, at any depth
fn glob_locks(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();

    for entry in fs::read_dir(dir)?.flatten() {
        let path = entry.path();

        if path.is_dir() {
            found.extend(glob_locks(&path).unwrap_or_default());
        } else if path.extension().is_some_and(|e| e == "lock") {
            found.push(path);
        }
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_picks_host_checks() {
        let checks = BTreeMap::from([
            ("lint".to_string(), "make lint".to_string()),
            ("test".to_string(), "make test".to_string()),
        ]);
        assert_eq!(
            select_host(&checks, &["test".into()], &[]).unwrap(),
            BTreeMap::from([(HOST.to_string(), BTreeSet::from(["test".to_string()]))])
        );
        assert!(
            select_host(&checks, &["nope".into()], &[])
                .unwrap_err()
                .contains("the checks are: lint, test")
        );
        assert!(select_host(&checks, &[], &["ci.yml".into()]).is_err());
    }

    #[test]
    fn last_run_finds_the_key_a_trial_base_gave_the_tree() {
        let home = tempfile::tempdir().unwrap();

        for (tree, key, base) in [
            ("t1", "main", "b1"),
            ("t1", "trial", "b2"),
            ("t2", "other", "b3"),
        ] {
            let line = serde_json::json!({"event": "run", "tree": tree, "key": key, "base": base});
            crate::record::append(home.path(), "o/r", &line).unwrap();
        }

        assert_eq!(
            last_run(home.path(), "o/r", "t1", |_| true),
            Some(("trial".into(), "b2".into()))
        );
        assert_eq!(
            last_run(home.path(), "o/r", "t1", |k| k == "main"),
            Some(("main".into(), "b1".into()))
        );
        assert_eq!(last_run(home.path(), "o/r", "t9", |_| true), None);
    }

    #[test]
    fn home_moves_once_and_never_merges() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("Library/Application Support/shipcheck");
        let new = dir.path().join(".local/state/shipcheck");
        assert_eq!(move_home(&old, &new).unwrap(), Migration::Nothing);

        fs::create_dir_all(old.join("logs")).unwrap();
        fs::write(old.join("logs/k"), "log").unwrap();
        assert_eq!(move_home(&old, &new).unwrap(), Migration::Moved);
        assert!(!old.exists());
        assert_eq!(fs::read_to_string(new.join("logs/k")).unwrap(), "log");
        assert_eq!(
            fs::metadata(&new).unwrap().permissions().mode() & 0o777,
            0o700
        );

        fs::create_dir_all(&old).unwrap();
        assert_eq!(move_home(&old, &new).unwrap(), Migration::Both);
        assert!(old.exists() && new.join("logs/k").exists());
    }

    #[test]
    fn a_relative_home_is_made_absolute() {
        if std::env::var_os("SHIPCHECK_HOME_CHILD").is_some() {
            assert_eq!(home(), std::env::current_dir().unwrap().join("rel/home"));
            return;
        }

        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "run::tests::a_relative_home_is_made_absolute"])
            .env("SHIPCHECK_HOME_CHILD", "1")
            .env("SHIPCHECK_HOME", "rel/home")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[test]
    fn an_empty_home_variable_counts_as_unset() {
        if std::env::var_os("SHIPCHECK_HOME_CHILD").is_some() {
            assert_eq!(home(), PathBuf::from("/tmp/fake/.local/state/shipcheck"));
            return;
        }

        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run::tests::an_empty_home_variable_counts_as_unset",
            ])
            .env("SHIPCHECK_HOME_CHILD", "1")
            .env("HOME", "/tmp/fake")
            .env("SHIPCHECK_HOME", "")
            .env("XDG_STATE_HOME", "")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[test]
    fn default_home_ignores_the_environment() {
        if std::env::var_os("SHIPCHECK_HOME_CHILD").is_some() {
            assert!(
                !default_home().starts_with("/tmp/fake"),
                "{}",
                default_home().display()
            );
            assert!(default_home().starts_with(user_dir()));
            assert!(home_overridden(), "HOME moved the results");
            return;
        }

        assert!(user_dir().is_absolute());
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run::tests::default_home_ignores_the_environment",
            ])
            .env("SHIPCHECK_HOME_CHILD", "1")
            .env("HOME", "/tmp/fake")
            .env("XDG_STATE_HOME", "/tmp/fake/state")
            .env_remove("SHIPCHECK_HOME")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[test]
    fn safe_git_config_follows_the_users_own() {
        let vars: BTreeMap<_, _> = safe_git_env(2).into_iter().collect();
        assert_eq!(vars["GIT_CONFIG_COUNT"], "8");
        assert_eq!(vars["GIT_CONFIG_KEY_2"], "filter.lfs.process");
        assert_eq!(vars["GIT_CONFIG_KEY_7"], "core.fsmonitor");
        assert_eq!(vars["GIT_CONFIG_VALUE_6"], "false");
        assert!(!vars.contains_key("GIT_CONFIG_KEY_1"));
    }

    #[test]
    fn secrets_longest_first_with_each_line() {
        let text = "# note\nA=abc\nB=\"abcdef\"\nC='x1\\ny2'\nnot a pair\nD=\n";
        assert_eq!(
            parse_secrets(text),
            ["abcdef", "x1\\ny2", "abc", "x1", "y2"]
        );
    }

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
    fn skip_variable_values() {
        assert!(env_skips(Some("1")));
        assert!(env_skips(Some("yes")));
        assert!(!env_skips(None));
        assert!(!env_skips(Some("")));
        assert!(!env_skips(Some("0")));
        assert!(!env_skips(Some("False")));
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

    #[test]
    fn a_partial_run_picks_jobs_legs_workflows_and_their_needs() {
        let g = |jobs: &[(&str, &[&str])]| -> Vec<(String, Vec<String>)> {
            jobs.iter()
                .map(|(j, n)| (j.to_string(), n.iter().map(|s| s.to_string()).collect()))
                .collect()
        };
        let graphs = BTreeMap::from([
            (
                "ci.yml".to_string(),
                g(&[
                    ("lint", &[]),
                    ("build", &[]),
                    ("test__1", &["build"]),
                    ("test__2", &["build"]),
                    ("tests", &[]),
                    ("deploy", &["test__1", "test__2"]),
                ]),
            ),
            ("docs.yml".to_string(), g(&[("docs", &[])])),
        ]);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let set = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>();

        let picked = select(&graphs, &s(&["test"]), &[]).unwrap();
        assert_eq!(picked.len(), 1, "only the workflow with the job runs");
        assert_eq!(picked["ci.yml"], set(&["build", "test__1", "test__2"]));

        let picked = select(&graphs, &s(&["deploy"]), &[]).unwrap();
        assert_eq!(
            picked["ci.yml"],
            set(&["build", "deploy", "test__1", "test__2"])
        );

        let picked = select(&graphs, &s(&["lint"]), &s(&[".github/workflows/docs.yml"])).unwrap();
        assert_eq!(picked["ci.yml"], set(&["lint"]));
        assert_eq!(picked["docs.yml"], set(&["docs"]));

        // The refusal names each job as --job takes it
        assert_eq!(
            select(&graphs, &s(&["nope"]), &[]).unwrap_err(),
            "no job nope, the jobs are: build (ci.yml/build), deploy (ci.yml/deploy), lint (ci.yml/lint), test (ci.yml/test, legs test__1, test__2), tests (ci.yml/tests), docs (docs.yml/docs)"
        );
        assert!(select(&graphs, &s(&["test__"]), &[]).is_err());
        assert!(select(&graphs, &[], &s(&["release.yml"])).is_err());

        // `<file>/<job>` names one file's job only
        let both = BTreeMap::from([
            ("ci.yml".to_string(), g(&[("test", &[])])),
            (
                "e2e.yml".to_string(),
                g(&[("build", &[]), ("test", &["build"])]),
            ),
        ]);
        let picked = select(&both, &s(&["e2e.yml/test"]), &[]).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked["e2e.yml"], set(&["build", "test"]));
        assert_eq!(select(&both, &s(&["test"]), &[]).unwrap().len(), 2);
        assert!(select(&both, &s(&["docs.yml/test"]), &[]).is_err());
        assert!(select(&both, &s(&["ci.yml/build"]), &[]).is_err());
    }

    #[test]
    fn a_config_places_each_job_on_one_runner() {
        let g = |jobs: &[(&str, &[&str])]| -> Vec<(String, Vec<String>)> {
            jobs.iter()
                .map(|(j, n)| (j.to_string(), n.iter().map(|s| s.to_string()).collect()))
                .collect()
        };
        let ci = "jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n  e2e:\n    needs: build\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";
        let set = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>();
        let jobs = |text: &str| {
            let cfg = RepoConfig::parse(text).unwrap();
            expected_jobs(&cfg, |f| (f == "ci.yml").then(|| ci.to_string()))
        };

        // An act job beside host checks brings the jobs it needs, and no other
        let mixed = jobs("runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n[jobs.\"ci.yml/e2e\"]\nrunner = \"act\"").unwrap();
        assert_eq!(mixed.len(), 2);
        assert_eq!(mixed[HOST], set(&["lint"]));
        assert_eq!(mixed["ci.yml"], set(&["build", "e2e"]));
        let names: Vec<String> = crate::check::job_names(&mixed)
            .into_iter()
            .map(|n| n.2)
            .collect();
        assert_eq!(
            names,
            ["shipcheck/build", "shipcheck/e2e", "shipcheck/lint"]
        );

        // Without [jobs], every job of a listed workflow, or only the host checks, as before
        let act = jobs("workflows = [\"ci.yml\"]").unwrap();
        assert_eq!(act.len(), 1);
        assert_eq!(act["ci.yml"], set(&["build", "e2e", "lint"]));
        let host = jobs("runner = \"host\"\n[host.checks]\nlint = \"make lint\"").unwrap();
        assert_eq!(host.len(), 1);
        assert_eq!(host[HOST], set(&["lint"]));

        // A host entry takes a job out of a listed workflow, and a host check of that name stands in
        let out = jobs("workflows = [\"ci.yml\"]\n[host.checks]\nlint = \"make lint\"\n[jobs.\"ci.yml/lint\"]\nrunner = \"host\"").unwrap();
        assert_eq!(out["ci.yml"], set(&["build", "e2e"]));
        let names: Vec<String> = crate::check::job_names(&out)
            .into_iter()
            .map(|n| n.2)
            .collect();
        assert_eq!(
            names,
            ["shipcheck/build", "shipcheck/e2e", "shipcheck/lint"]
        );
        let listed = "workflows = [\"ci.yml\"]\n";
        let entry = |id: &str| format!("[jobs.\"ci.yml/{id}\"]\nrunner = \"host\"\n");
        let checks = |names: &[&str]| {
            let lines: Vec<String> = names.iter().map(|n| format!("{n} = \"x\"\n")).collect();
            format!("[host.checks]\n{}", lines.concat())
        };
        let err = |text: String| jobs(&text).unwrap_err();

        assert!(
            err(format!("{listed}{}{}", checks(&["build"]), entry("build")))
                .contains("act job ci.yml/e2e needs build")
        );
        assert!(
            err(format!("{listed}{}{}", checks(&["lnit"]), entry("lnit")))
                .contains("\"ci.yml/lnit\" names no job of ci.yml")
        );
        assert!(err(format!("{listed}{}", entry("lint"))).contains("no check \"lint\""));
        assert!(
            err(format!("{listed}{}{}", checks(&["test"]), entry("lint")))
                .contains("no check \"lint\"")
        );
        // Both moved at once, whatever order the entries come in
        let both = jobs(&format!(
            "{listed}{}{}{}",
            checks(&["build", "e2e"]),
            entry("e2e"),
            entry("build")
        ))
        .unwrap();
        assert_eq!(both["ci.yml"], set(&["lint"]));
        assert_eq!(both[HOST], set(&["build", "e2e"]));
        let all = format!(
            "{listed}{}{}{}{}",
            checks(&["build", "e2e", "lint"]),
            entry("build"),
            entry("e2e"),
            entry("lint")
        );
        assert!(err(all).contains("every job of ci.yml is on the host runner"));
        assert!(jobs("runner = \"host\"\n[host.checks]\nt = \"x\"\n[jobs.\"ci.yml/nope\"]\nrunner = \"act\"").is_err());

        let graphs = BTreeMap::from([(
            "ci.yml".to_string(),
            g(&[("lint__1", &[]), ("lint__2", &[]), ("test", &[])]),
        )]);
        let cfg =
            RepoConfig::parse(&format!("{listed}{}{}", checks(&["lint"]), entry("lint"))).unwrap();
        assert_eq!(
            act_jobs(&cfg, &graphs).unwrap()["ci.yml"],
            set(&["test"]),
            "a host entry takes every leg"
        );
    }

    #[test]
    fn slots_are_exclusive_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let (first, lock) = claim_slot(dir.path()).unwrap();
        let (second, _keep) = claim_slot(dir.path()).unwrap();
        assert_ne!(first, second, "a held slot must not be handed out twice");
        drop(lock);
        // A child another test is spawning can hold the lock's descriptor until it execs
        let again = (0..50)
            .map(|_| {
                let (slot, _) = claim_slot(dir.path()).unwrap();
                std::thread::sleep(Duration::from_millis(10));
                slot
            })
            .find(|slot| *slot == first)
            .unwrap_or_default();
        assert_eq!(
            again, first,
            "a freed slot must be reused, to keep warm paths stable"
        );
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_checkout_passes_only_with_the_tested_tree_on_disk() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("job");
        fs::create_dir(&repo).unwrap();
        sh(&repo, &["init", "-q"]);
        fs::write(repo.join("a"), "a").unwrap();
        fs::write(repo.join("e"), "e").unwrap();
        fs::set_permissions(repo.join("e"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("a", repo.join("l")).unwrap();
        fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "i"]);
        let tree = sh(&repo, &["rev-parse", "HEAD^{tree}"]);
        let restore = || sh(&repo, &["checkout", "-q", "--", "."]);

        // Build output, untracked files and new mtimes are not the code
        fs::create_dir(repo.join("target")).unwrap();
        fs::write(repo.join("target/stale"), "x").unwrap();
        fs::write(repo.join("untracked"), "x").unwrap();
        fs::File::open(repo.join("a"))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        verify_checkout(&repo, &tree).unwrap();
        assert!(!repo.with_extension("verify-index").exists());

        let tampers: [(&str, &dyn Fn()); 4] = [
            ("edited", &|| fs::write(repo.join("a"), "b").unwrap()),
            ("deleted", &|| fs::remove_file(repo.join("a")).unwrap()),
            ("mode", &|| {
                fs::set_permissions(repo.join("e"), fs::Permissions::from_mode(0o644)).unwrap()
            }),
            ("relinked", &|| {
                fs::remove_file(repo.join("l")).unwrap();
                symlink("e", repo.join("l")).unwrap();
            }),
        ];
        for (name, tamper) in tampers {
            tamper();
            assert!(verify_checkout(&repo, &tree).is_err(), "{name} must fail");
            restore();
            verify_checkout(&repo, &tree).unwrap();
        }

        let other = sh(&repo, &["rev-parse", "HEAD^{tree}"]).replace(char::is_alphanumeric, "0");
        // A wrong tree is the check's failure, exit 1, never "could not run"
        let e = verify_checkout(&repo, &other).unwrap_err();
        assert_eq!(error_state(&e), WRONG_TREE, "{e}");
    }

    #[test]
    fn a_wrong_tree_fails_the_check_and_is_never_kept() {
        let entry: Entry = serde_json::from_value(serde_json::json!({
            "key": "k", "sha": "s", "tree": "t", "base": "origin/main", "passed": false,
            "finished_unix": 0,
            "workflows": [{
                "file": HOST, "exit": 1, "attempts": 1, "log_sha256": "", "passed": false,
                "jobs": {"test": WRONG_TREE, "fmt": "success"},
            }],
        }))
        .unwrap();
        assert!(entry.wrong_tree() && !entry.could_not_run());
        assert_eq!(exit_code(&entry), 1);
    }

    #[test]
    fn asks_the_remote_for_its_default_branch_once_per_process() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let (origin, repo) = (dir.path().join("origin.git"), dir.path().join("repo"));
        sh(
            dir.path(),
            &["init", "-q", "--bare", "-b", "trunk", "origin.git"],
        );
        sh(dir.path(), &["init", "-q", "-b", "trunk", "repo"]);
        sh(&repo, &["commit", "-q", "--allow-empty", "-m", "trunk"]);
        sh(
            &repo,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        sh(&repo, &["push", "-q", "origin", "trunk"]);
        // Each ls-remote of a local remote starts its upload-pack, which counts itself
        let count = dir.path().join("asked");
        let pack = dir.path().join("upload-pack");
        let script = format!(
            "#!/bin/sh\necho >> {}\nexec git upload-pack \"$@\"\n",
            count.display()
        );
        fs::write(&pack, script).unwrap();
        fs::set_permissions(&pack, fs::Permissions::from_mode(0o755)).unwrap();
        sh(
            &repo,
            &[
                "config",
                "remote.origin.uploadpack",
                &pack.to_string_lossy(),
            ],
        );

        for _ in 0..3 {
            assert_eq!(remote_base(&repo).as_deref(), Some("origin/trunk"));
        }

        assert_eq!(fs::read_to_string(&count).unwrap().lines().count(), 1);
    }

    #[test]
    fn commit_and_post_refuse_the_default_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        sh(repo, &["init", "-q", "-b", "main"]);
        sh(repo, &["commit", "-q", "--allow-empty", "-m", "main"]);
        let main = sh(repo, &["rev-parse", "HEAD"]);
        let main_branch = Some("main".to_string());
        assert_eq!(on_default_branch(repo, "origin/main", None), main_branch);
        assert_eq!(
            on_default_branch(repo, "origin/main", Some(&main)),
            main_branch
        );

        sh(repo, &["switch", "-q", "-c", "feature"]);
        sh(repo, &["commit", "-q", "--allow-empty", "-m", "feature"]);
        let feature = sh(repo, &["rev-parse", "HEAD"]);
        assert_eq!(on_default_branch(repo, "origin/main", None), None);
        assert_eq!(on_default_branch(repo, "origin/main", Some(&feature)), None);

        // A PR's commit posts from anywhere, main's tip from nowhere
        sh(repo, &["switch", "-q", "--detach", &feature]);
        assert_eq!(on_default_branch(repo, "origin/main", Some(&feature)), None);
        assert_eq!(
            on_default_branch(repo, "origin/main", Some(&main)),
            main_branch
        );
        sh(repo, &["switch", "-q", "main"]);
        assert_eq!(on_default_branch(repo, "origin/main", Some(&feature)), None);

        // The remote's tip counts too, and `base` names the branch
        sh(repo, &["update-ref", "refs/remotes/origin/trunk", &feature]);
        assert_eq!(
            on_default_branch(repo, "origin/trunk", Some(&feature)),
            Some("trunk".into())
        );
    }

    #[test]
    fn removes_planted_links_and_keeps_their_targets() {
        let dir = tempfile::tempdir().unwrap();
        let (host, checkout) = (dir.path().join("host"), dir.path().join("checkout"));
        fs::create_dir_all(host.join("workflows")).unwrap();
        fs::write(host.join("zshrc"), "mine").unwrap();
        fs::create_dir_all(&checkout).unwrap();
        std::os::unix::fs::symlink(&host, checkout.join(".github")).unwrap();
        std::os::unix::fs::symlink(host.join("zshrc"), checkout.join(CONFIG)).unwrap();

        remove_planted_links(&checkout).unwrap();
        assert!(!checkout.join(".github").exists());
        assert!(!checkout.join(CONFIG).exists());
        assert_eq!(fs::read_to_string(host.join("zshrc")).unwrap(), "mine");
        assert!(host.join("workflows").is_dir());
    }

    #[test]
    fn a_shallow_repository_gives_the_job_a_history_it_can_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, clone, job) = (
            tmp.path().join("repo"),
            tmp.path().join("clone"),
            tmp.path().join("job"),
        );
        fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);

        for n in ["a", "b", "c"] {
            sh(&repo, &["commit", "-q", "--allow-empty", "-m", n]);
        }

        let url = format!("file://{}", repo.display());
        let clone_s = clone.to_string_lossy().into_owned();
        sh(tmp.path(), &["clone", "-q", "--depth", "2", &url, &clone_s]);
        let sha = sh(&clone, &["rev-parse", "HEAD"]);
        let job_s = job.to_string_lossy().into_owned();
        sh(
            &clone,
            &["worktree", "add", "-q", "--detach", &job_s, "HEAD"],
        );

        standalone_git(&job, &sha).unwrap();
        assert_eq!(sh(&job, &["rev-list", "--count", "HEAD"]), "2");
    }

    #[test]
    fn what_a_job_writes_in_its_checkout_runs_nothing_on_the_host() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = tempfile::tempdir().unwrap();
        let (repo, host) = (tmp.path().join("repo"), tmp.path().join("host"));
        let (template, job) = (tmp.path().join("template"), tmp.path().join("job"));
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(host.join("dir")).unwrap();
        fs::write(host.join("secret"), "mine").unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("a.txt"), "a").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "a"]);
        let sha = sh(&repo, &["rev-parse", "HEAD"]);

        for dir in [&template, &job] {
            let dir = dir.to_string_lossy();
            sh(&repo, &["worktree", "add", "-q", "--detach", &dir, "HEAD"]);
        }

        let link = standalone_git(&job, &sha).unwrap();

        // The job: git config that names commands, hooks, a filter, and links out of the checkout
        let ran = |what: &str| tmp.path().join(format!("ran-{what}"));
        let touch = |what: &str| format!("touch '{}'", ran(what).display());
        let config = format!(
            "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n\tpager = {}\n[filter \"x\"]\n\tclean = {}\n\tsmudge = {}\n\tprocess = {}\n[diff \"x\"]\n\ttextconv = {}\n",
            touch("fsmonitor"),
            job.join(".git/hooks").display(),
            touch("pager"),
            touch("clean"),
            touch("smudge"),
            touch("process"),
            touch("textconv"),
        );
        let mut text = fs::read_to_string(job.join(".git/config")).unwrap();
        text.push_str(&config);
        fs::write(job.join(".git/config"), text).unwrap();
        fs::create_dir_all(job.join(".git/hooks")).unwrap();

        for hook in [
            "post-checkout",
            "post-index-change",
            "reference-transaction",
            "pre-auto-gc",
        ] {
            let path = job.join(".git/hooks").join(hook);
            fs::write(&path, format!("#!/bin/sh\n{}\n", touch("hook"))).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fs::write(job.join(".gitattributes"), "* filter=x diff=x\n").unwrap();
        fs::write(job.join(".gitignore"), "target\nout\n").unwrap();
        fs::create_dir_all(job.join("target")).unwrap();
        symlink(host.join("secret"), job.join("target/secret")).unwrap();
        symlink(host.join("dir"), job.join("target/dir")).unwrap();
        symlink(&host, job.join("out")).unwrap();
        fs::hard_link(job.join("a.txt"), job.join("target/hard")).unwrap();

        finish_job(&job, &template, link.as_deref(), &|_| {});

        let left: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("ran-"))
            .collect();
        assert!(
            left.is_empty(),
            "the job's checkout ran {left:?} on the host"
        );
        assert_eq!(fs::read_to_string(host.join("secret")).unwrap(), "mine");
        assert!(host.join("dir").is_dir());
        assert_eq!(fs::read_dir(&job).unwrap().count(), 0);
        // Warming still works, through the host's worktree link. Linux temp dirs often cannot clone
        if cfg!(target_os = "macos") {
            assert!(
                template.join("target").is_dir(),
                "the template was not warmed"
            );
        }
    }

    #[test]
    fn git_on_a_job_checkout_never_reads_its_git() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, job) = (tmp.path().join("repo"), tmp.path().join("job"));
        fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("a.txt"), "a").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "a"]);
        sh(&repo, &["tag", "v1"]);
        let job_s = job.to_string_lossy();
        sh(
            &repo,
            &["worktree", "add", "-q", "--detach", &job_s, "HEAD"],
        );
        let link = fs::read_to_string(job.join(".git")).unwrap();
        let gitdir = PathBuf::from(link.trim().strip_prefix("gitdir: ").unwrap());

        // A job swapped `.git` for its own, and even the host's config names a monitor
        let ran = tmp.path().join("ran");
        let touch = format!("[core]\n\tfsmonitor = touch '{}'\n", ran.display());
        fs::remove_file(job.join(".git")).unwrap();
        fs::create_dir_all(job.join(".git")).unwrap();
        sh(&job, &["init", "-q"]);
        let own = fs::read_to_string(job.join(".git/config")).unwrap();
        fs::write(job.join(".git/config"), format!("{own}{touch}")).unwrap();
        let host = fs::read_to_string(repo.join(".git/config")).unwrap();
        fs::write(repo.join(".git/config"), format!("{host}{touch}")).unwrap();

        let tags = job_git(&job, &gitdir, &["tag", "--merged", "HEAD"]).unwrap();
        assert_eq!(tags, "v1", "git read the job's repository");
        job_git(&job, &gitdir, &["status", "--porcelain"]).unwrap();
        assert!(!ran.exists(), "git ran a monitor on the host");
    }

    #[test]
    fn snapshots_the_working_copy_and_the_index_and_changes_neither() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        sh(repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("a.txt"), "a").unwrap();
        fs::write(repo.join("b.txt"), "b").unwrap();
        fs::write(repo.join(".gitignore"), "junk\n").unwrap();
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "main"]);
        assert_eq!(snapshot(repo, false).unwrap(), None, "clean means HEAD");
        assert_eq!(snapshot(repo, true).unwrap(), None);

        fs::write(repo.join("a.txt"), "stashed").unwrap();
        sh(repo, &["stash", "-q"]);
        fs::write(repo.join("a.txt"), "a2").unwrap();
        sh(repo, &["add", "a.txt"]);
        fs::write(repo.join("a.txt"), "a3").unwrap();
        fs::remove_file(repo.join("b.txt")).unwrap();
        fs::write(repo.join("new.txt"), "n").unwrap();
        fs::write(repo.join("junk"), "j").unwrap();
        let state = || {
            (
                sh(repo, &["--no-optional-locks", "status", "--porcelain"]),
                fs::read(repo.join(".git/index")).unwrap(),
                sh(repo, &["for-each-ref"]),
                sh(repo, &["stash", "list"]),
            )
        };
        let before = state();
        assert_eq!(untracked(repo).unwrap(), ["new.txt"], "junk is ignored");

        let wc = snapshot(repo, false).unwrap().unwrap();
        let files = sh(repo, &["ls-tree", "-r", "--name-only", &wc]);
        assert_eq!(files, ".gitignore\na.txt\nnew.txt");
        assert_eq!(sh(repo, &["show", &format!("{wc}:a.txt")]), "a3");
        assert_eq!(
            sh(repo, &["rev-parse", &format!("{wc}^")]),
            sh(repo, &["rev-parse", "HEAD"])
        );

        let staged = snapshot(repo, true).unwrap().unwrap();
        let files = sh(repo, &["ls-tree", "-r", "--name-only", &staged]);
        assert_eq!(files, ".gitignore\na.txt\nb.txt");
        assert_eq!(sh(repo, &["show", &format!("{staged}:a.txt")]), "a2");

        assert!(
            before == state(),
            "the index, refs and stash must not change"
        );
        let left: Vec<_> = fs::read_dir(repo.join(".git"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("shipcheck-index")
            })
            .collect();
        assert!(left.is_empty(), "the temporary index must go");

        // Committing all of it gives the tree that ran, so the result is found by tree
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "all"]);
        assert_eq!(
            sh(repo, &["rev-parse", "HEAD^{tree}"]),
            sh(repo, &["rev-parse", &format!("{wc}^{{tree}}")])
        );
        assert_eq!(snapshot(repo, false).unwrap(), None);
    }

    #[test]
    fn the_index_copy_keeps_the_time_the_index_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let (index, temp) = (dir.path().join("index"), dir.path().join("copy"));
        fs::write(&index, "entries").unwrap();
        let written = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::File::options()
            .write(true)
            .open(&index)
            .unwrap()
            .set_modified(written)
            .unwrap();
        copy_index(&index, &temp).unwrap();
        assert_eq!(fs::read(&temp).unwrap(), b"entries");
        assert_eq!(fs::metadata(&temp).unwrap().modified().unwrap(), written);
    }

    #[test]
    fn commit_runs_the_users_hooks_with_the_callers_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        // A tool in a git checkout, as ~/.nvm is, which the trusted PATH drops
        let tools = dir.path().join("nvm/bin");
        fs::create_dir_all(dir.path().join("nvm/.git")).unwrap();
        fs::create_dir_all(&tools).unwrap();
        fs::create_dir_all(&repo).unwrap();
        fs::write(tools.join("husky-tool"), "#!/bin/sh\ntouch \"$1\"\n").unwrap();
        // A fake git on the caller's PATH, which the commit itself must never run
        let faked = dir.path().join("fake-git-ran");
        let fake = format!("#!/bin/sh\ntouch {}\nexit 1\n", faked.display());
        fs::write(tools.join("git"), fake).unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(tools.join("husky-tool"))
            .arg(tools.join("git"))
            .status()
            .unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "user.email", "t@t"]);
        sh(&repo, &["config", "commit.gpgsign", "false"]);
        sh(&repo, &["commit", "-q", "--allow-empty", "-m", "main"]);
        let hook = repo.join(".git/hooks/pre-commit");
        let ran = dir.path().join("tool-ran");
        fs::write(&hook, format!("#!/bin/sh\nhusky-tool {}\n", ran.display())).unwrap();
        Command::new("chmod").arg("+x").arg(&hook).status().unwrap();
        fs::write(repo.join("a.txt"), "a").unwrap();
        let wc = snapshot(&repo, false).unwrap().unwrap();
        let tested = Tested {
            tree: sh(&repo, &["rev-parse", &format!("{wc}^{{tree}}")]),
            staged: false,
            passed: true,
            partial: false,
        };
        let path = std::env::join_paths(
            std::iter::once(tools).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        CALLER_PATH.set(path).unwrap();

        commit(&repo, &tested, &["-m".into(), "a".into()], false).unwrap();
        assert!(ran.exists(), "the hook found its tool on the caller's PATH");
        assert!(!faked.exists(), "git came from the trusted PATH");
    }

    #[test]
    fn commits_only_the_tree_that_ran() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        sh(repo, &["init", "-q", "-b", "main"]);
        sh(repo, &["config", "user.name", "t"]);
        sh(repo, &["config", "user.email", "t@t"]);
        sh(repo, &["config", "commit.gpgsign", "false"]);
        fs::write(repo.join("a.txt"), "a").unwrap();
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "main"]);
        let hook = repo.join(".git/hooks/pre-commit");
        fs::write(
            &hook,
            "#!/bin/sh\ntouch \"$(git rev-parse --git-dir)/hook-ran\"\n",
        )
        .unwrap();
        Command::new("chmod").arg("+x").arg(&hook).status().unwrap();

        fs::write(repo.join("a.txt"), "a2").unwrap();
        sh(repo, &["add", "a.txt"]);
        fs::write(repo.join("a.txt"), "a3").unwrap();
        fs::write(repo.join("b.txt"), "b").unwrap();
        let wc = snapshot(repo, false).unwrap().unwrap();
        let tested = Tested {
            tree: sh(repo, &["rev-parse", &format!("{wc}^{{tree}}")]),
            staged: false,
            passed: true,
            partial: false,
        };
        let msg = ["-m".to_string(), "all".to_string()];
        let head = sh(repo, &["rev-parse", "HEAD"]);
        let index = || fs::read(repo.join(".git/index")).unwrap();
        let before = index();

        let failed = Tested {
            passed: false,
            ..tested.clone()
        };
        assert!(
            commit(repo, &failed, &msg, false)
                .unwrap_err()
                .contains("failed")
        );

        // A partial run that passed is still not the check, and the refusal says so
        let partial = Tested {
            partial: true,
            ..tested.clone()
        };
        let err = commit(repo, &partial, &msg, true).unwrap_err();
        assert!(err.starts_with("the last run was partial"), "{err}");

        fs::write(repo.join("c.txt"), "after the run").unwrap();
        let err = commit(repo, &tested, &msg, false).unwrap_err();
        assert!(err.contains("files changed since the run"), "{err}");
        assert_eq!(sh(repo, &["rev-parse", "HEAD"]), head);
        assert!(index() == before, "a refusal keeps the staged state");
        fs::remove_file(repo.join("c.txt")).unwrap();

        let sha = commit(repo, &tested, &msg, false).unwrap();
        assert_eq!(sh(repo, &["rev-parse", "HEAD"]), sha);
        assert_eq!(sh(repo, &["rev-parse", "HEAD^{tree}"]), tested.tree);
        assert_eq!(sh(repo, &["rev-parse", "HEAD^"]), head);
        assert_eq!(sh(repo, &["log", "-1", "--format=%s"]), "all");
        assert_eq!(
            sh(repo, &["status", "--porcelain"]),
            "",
            "the index follows"
        );
        assert!(repo.join(".git/hook-ran").exists(), "git's own hooks run");

        // With --staged, only the index goes in
        fs::write(repo.join("a.txt"), "a4").unwrap();
        sh(repo, &["add", "a.txt"]);
        fs::write(repo.join("d.txt"), "left out").unwrap();
        let staged = snapshot(repo, true).unwrap().unwrap();
        let tested = Tested {
            tree: sh(repo, &["rev-parse", &format!("{staged}^{{tree}}")]),
            staged: true,
            passed: true,
            partial: false,
        };
        commit(repo, &tested, &msg, false).unwrap();
        assert_eq!(sh(repo, &["rev-parse", "HEAD^{tree}"]), tested.tree);
        assert_eq!(sh(repo, &["status", "--porcelain"]), "?? d.txt");
    }

    #[test]
    fn names_ci_changes_since_the_branch_left_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let ci = |coe: &str, extra: &str| {
            format!(
                "on: push\njobs:\n  test:{coe}\n    runs-on: x\n    steps: [{{run: make}}]\n{extra}"
            )
        };
        let write = |body: String| {
            fs::create_dir_all(repo.join(WORKFLOWS)).unwrap();
            fs::write(repo.join(WORKFLOWS).join("ci.yml"), body).unwrap();
            sh(repo, &["add", "-A"]);
        };
        sh(repo, &["init", "-q", "-b", "main"]);
        write(ci("", ""));
        sh(repo, &["commit", "-qm", "main"]);
        sh(repo, &["checkout", "-q", "-b", "pr"]);
        write(ci("\n    continue-on-error: true", ""));
        fs::write(repo.join(CONFIG), "workflows = []").unwrap();
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "weaken"]);
        let sha = sh(repo, &["rev-parse", "HEAD"]);
        // main moves on with a job of its own, which the branch must not be blamed for removing
        sh(repo, &["checkout", "-q", "main"]);
        write(ci("", "  lint: {runs-on: x, steps: [{run: lint}]}\n"));
        sh(repo, &["commit", "-qm", "lint"]);

        assert_eq!(
            ci_changes(repo, "main", &sha).unwrap(),
            [
                "`.github/workflows/ci.yml` changes",
                "`.github/workflows/ci.yml`: job `test` gets continue-on-error",
                "`shipcheck.toml` is added",
            ]
        );
        assert!(ci_changes(repo, "main", "main").unwrap().is_empty());
    }

    #[test]
    fn names_changes_to_what_commands_run() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let pkg = |test: &str, dep: &str| {
            format!(
                "{{\"scripts\": {{\"test\": \"{test}\"}}, \"dependencies\": {{\"a\": \"{dep}\"}}}}"
            )
        };
        sh(repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(repo.join("web")).unwrap();
        fs::write(repo.join("package.json"), pkg("jest", "1")).unwrap();
        fs::write(repo.join("web/package.json"), pkg("jest", "1")).unwrap();
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "main"]);
        sh(repo, &["checkout", "-q", "-b", "pr"]);
        fs::write(repo.join("package.json"), pkg("jest", "2")).unwrap();
        fs::write(repo.join("web/package.json"), pkg("true", "1")).unwrap();
        fs::write(repo.join("Makefile"), "test:\n\ttrue\n").unwrap();
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "cheat"]);
        let sha = sh(repo, &["rev-parse", "HEAD"]);

        assert_eq!(
            ci_changes(repo, "main", &sha).unwrap(),
            [
                "`Makefile` is added, it can change what checks run",
                "`web/package.json` changes what checks may run",
            ]
        );
    }

    /// A repository with `src/lib.rs` and `docs/a.md` on main, and a plan for the commit
    /// `change` makes on top of it
    fn plan_after(change: impl Fn(&Path)) -> (tempfile::TempDir, Plan) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        sh(repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(repo.join("src/lib.rs"), "fn a() {}").unwrap();
        fs::write(repo.join("docs/a.md"), "a").unwrap();
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "main"]);
        let base = sh(repo, &["rev-parse", "HEAD"]);
        change(repo);
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "change"]);
        let sha = sh(repo, &["rev-parse", "HEAD"]);
        let config_text = b"workflows = [\"ci.yml\"]\npaths = [\"src/**\"]\n".to_vec();
        let plan = Plan {
            repo: repo.to_path_buf(),
            tree: sh(repo, &["rev-parse", "HEAD^{tree}"]),
            sha,
            base,
            config: RepoConfig::parse(&String::from_utf8_lossy(&config_text)).unwrap(),
            config_text,
            workflows: BTreeMap::new(),
            key: "k".into(),
            ci_changes: Vec::new(),
            defaults: false,
            tools: String::new(),
        };
        (dir, plan)
    }

    #[test]
    fn a_stored_result_counts_only_when_what_it_shows_holds() {
        if std::env::var_os("SHIPCHECK_HOME_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let out = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "run::tests::a_stored_result_counts_only_when_what_it_shows_holds",
                ])
                .env("SHIPCHECK_HOME_CHILD", "1")
                .env("SHIPCHECK_HOME", home.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
            return;
        }

        let (_d, plan) = plan_after(|r| fs::write(r.join("src/lib.rs"), "fn b() {}").unwrap());
        let store = |tree: &str, base: &str, log: &str| {
            let entry = serde_json::json!({
                "key": plan.key, "sha": plan.sha, "tree": tree, "base": base,
                "passed": true, "finished_unix": 0,
                "workflows": [{"file": HOST, "exit": 0, "attempts": 1, "jobs": {"t": "success"},
                    "passed": true, "log_sha256": cache::hex(&Sha256::digest(b"the log"))}]
            });
            fs::create_dir_all(home().join("cache")).unwrap();
            fs::write(
                home().join(format!("cache/{}.json", plan.key)),
                entry.to_string(),
            )
            .unwrap();
            fs::create_dir_all(log_dir(&plan.key)).unwrap();
            fs::write(log_dir(&plan.key).join(format!("{HOST}.log")), log).unwrap();
        };
        store(&plan.tree, &plan.base, "the log");
        assert!(lookup(&plan).is_some());
        store(&"0".repeat(40), &plan.base, "the log");
        assert!(
            lookup(&plan).is_none(),
            "a result of another tree under this key"
        );
        store(&plan.tree, &plan.sha, "the log");
        assert!(
            lookup(&plan).is_none(),
            "a base that is not on the plan's base"
        );
        store(&plan.tree, &plan.base, "another log");
        assert!(
            lookup(&plan).is_none(),
            "a log that does not hash to its digest"
        );
        fs::remove_file(log_dir(&plan.key).join(format!("{HOST}.log"))).unwrap();
        assert!(lookup(&plan).is_none(), "a missing log");
    }

    #[test]
    fn paths_skip_only_when_nothing_under_paths_changed() {
        let (_d, docs_only) = plan_after(|r| fs::write(r.join("docs/a.md"), "b").unwrap());
        assert!(skipped(&docs_only).is_some());

        let (_d, edited) = plan_after(|r| fs::write(r.join("src/lib.rs"), "fn b() {}").unwrap());
        assert!(skipped(&edited).is_none());

        let (_d, deleted) = plan_after(|r| fs::remove_file(r.join("src/lib.rs")).unwrap());
        assert!(
            skipped(&deleted).is_none(),
            "deleting a file under paths must run"
        );

        let (_d, moved_out) = plan_after(|r| {
            fs::rename(r.join("src/lib.rs"), r.join("docs/lib.rs")).unwrap();
        });
        assert!(
            skipped(&moved_out).is_none(),
            "moving a file out of paths must run"
        );

        let (_d, unicode) = plan_after(|r| fs::write(r.join("src/café é.rs"), "x").unwrap());
        assert!(
            skipped(&unicode).is_none(),
            "a quoted name must still match paths"
        );
    }

    #[test]
    fn escapes_control_characters_for_the_terminal() {
        let out = printable("ok\ttab\n\x1b]0;title\x07\x1b[2J\rback\u{9b}31m\x7f");
        assert_eq!(
            out,
            "ok\ttab\n\\u{1b}]0;title\\u{7}\\u{1b}[2J\\rback\\u{9b}31m\\u{7f}"
        );
        assert!(
            !out.chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        );
    }

    #[test]
    fn a_job_waits_out_the_engine_view_of_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("0");
        let stamp = ended_stamp(&slot, &slot.join("ci.yml/test"));
        assert_eq!(stamp, dir.path().join("0.ended/ci.yml/test"));
        assert_eq!(ended_stamp(&slot, &slot), dir.path().join("0.ended/slot"));

        let now = SystemTime::now();
        assert_eq!(engine_wait(&stamp, now), Duration::ZERO, "no job ran there");
        mark_ended(&stamp);
        let wait = engine_wait(&stamp, SystemTime::now());
        assert!(
            wait > Duration::from_millis(1500) && wait <= ENGINE_VIEW,
            "{wait:?}"
        );
        let later = SystemTime::now() + Duration::from_secs(3);
        assert_eq!(engine_wait(&stamp, later), Duration::ZERO);
        let earlier = SystemTime::now() - Duration::from_secs(60);
        assert_eq!(engine_wait(&stamp, earlier), ENGINE_VIEW);
        // A new run deletes the slot, the stamps beside it stay
        fs::create_dir_all(slot.join("ci.yml/test")).unwrap();
        fs::remove_dir_all(&slot).unwrap();
        assert!(stamp.exists());
    }

    #[test]
    fn a_failed_attempt_keeps_its_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("test.log");
        fs::write(&log, "attempt 1").unwrap();
        let kept = keep_attempt_log(&log, 1).unwrap();
        assert_eq!(kept, dir.path().join("test.attempt-1.log"));
        fs::write(&log, "attempt 2").unwrap();
        assert_eq!(fs::read_to_string(kept).unwrap(), "attempt 1");
    }

    #[test]
    fn skip_markers_follow_github() {
        assert!(message_skips("wip [skip ci]"));
        assert!(message_skips("Docs only\n\n[CI SKIP]"));
        assert!(message_skips("[skip shipcheck] try something"));
        assert!(!message_skips("skip ci when docs change"));
        assert!(!message_skips("fix the skip list"));
    }

    #[test]
    fn prune_removes_only_old_results() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("cache")).unwrap();
        fs::create_dir_all(home.path().join("logs/oldkey")).unwrap();
        fs::create_dir_all(home.path().join("logs/monthkey")).unwrap();
        fs::write(home.path().join("cache/old.json"), "{}").unwrap();
        fs::write(home.path().join("cache/new.json"), "{}").unwrap();
        let days = |n: u64| SystemTime::now() - Duration::from_secs(n * 24 * 3600);

        for (p, age) in [
            ("cache/old.json", 30),
            ("logs/monthkey", 30),
            ("logs/oldkey", 100),
        ] {
            fs::File::open(home.path().join(p))
                .unwrap()
                .set_modified(days(age))
                .unwrap();
        }

        prune(home.path(), SystemTime::now());
        assert!(!home.path().join("cache/old.json").exists());
        assert!(!home.path().join("logs/oldkey").exists());
        assert!(
            home.path().join("logs/monthkey").exists(),
            "logs stay 90 days"
        );
        assert!(home.path().join("cache/new.json").exists());
    }

    type Marks<'a> = &'a [(&'a str, u32, u32, bool)];

    fn marks(of: Marks) -> BTreeMap<String, workflow::JobMarks> {
        of.iter()
            .map(|&(job, started, ended, failed)| {
                let m = workflow::JobMarks {
                    started,
                    ended,
                    failed,
                };
                (job.to_string(), m)
            })
            .collect()
    }

    #[test]
    fn job_state_from_the_act_exit_and_the_markers() {
        let (ok, cut, bad) = (
            ("test", 1, 1, false),
            ("test", 1, 0, false),
            ("test", 1, 1, true),
        );
        let no_image = "no image for its runs-on label on this host";
        let cases: &[(Option<i32>, bool, Marks, bool, &str)] = &[
            (Some(0), false, &[ok], false, "success"),
            (Some(0), false, &[("test", 2, 2, false)], false, "success"),
            // A job whose `if:` is false prints no markers
            (Some(0), false, &[], false, "skipped"),
            (Some(0), false, &[cut], false, "killed"),
            (Some(0), false, &[("test", 2, 1, false)], false, "killed"),
            (Some(0), false, &[bad], false, "failure"),
            (Some(0), false, &[("other", 1, 1, false)], false, "failure"),
            (
                Some(0),
                false,
                &[ok, ("other", 1, 0, false)],
                false,
                "failure",
            ),
            (Some(1), false, &[ok], false, "failure"),
            (Some(1), false, &[], false, "failure"),
            (Some(1), false, &[cut], false, "killed"),
            (None, false, &[ok], false, "timed out"),
            (Some(0), true, &[ok], false, no_image),
            (Some(0), false, &[ok], true, "success"),
            (Some(0), false, &[], true, "skipped"),
            (
                Some(1),
                false,
                &[bad],
                true,
                "failure, allowed by continue-on-error",
            ),
            (Some(0), true, &[], true, no_image),
        ];

        for (exit, unsupported, of, allow, want) in cases {
            assert_eq!(
                job_state(
                    "test",
                    *exit,
                    of.is_empty(),
                    *unsupported,
                    &marks(of),
                    *allow
                ),
                *want,
                "exit {exit:?}, unsupported {unsupported}, marks {of:?}, allowed {allow}"
            );
        }

        // A job act ran none of, with no word that its `if:` skipped it, is not skipped
        assert_eq!(
            job_state("test", Some(0), false, false, &marks(&[]), false),
            "failure, act ran nothing"
        );
    }

    #[test]
    fn a_skip_stands_only_when_the_if_is_false_on_every_pr() {
        let job = |id: &str, cond: &str| {
            format!(
                "  {id}:\n    if: {cond}\n    runs-on: ubuntu-latest\n    steps:\n      - run: make\n"
            )
        };
        let gate = "(github.ref == 'refs/heads/main' || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)";
        let wf = [
            "on: [push, pull_request, workflow_dispatch]\njobs:\n  label:\n    if: contains(github.event.pull_request.labels.*.name, 'slow')\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        os: [a, b]\n    steps:\n      - run: make slow\n".to_string(),
            job("dispatch", "github.event_name != 'workflow_dispatch'"),
            job("vars", "vars.RUN_E2E == 'true'"),
            job("either", "github.event_name == 'push' || github.event_name == 'pull_request'"),
            job("not_tag", "${{ !startsWith(github.ref, 'refs/tags/') }}"),
            job("push", "github.event_name == 'push'"),
            job("release", "github.event_name == 'push' && github.ref == format('refs/heads/{0}', github.event.repository.default_branch)"),
            job("tag", "startsWith(github.ref, 'refs/tags/')"),
            job("off", "false"),
            job("off_expr", "${{ false }}"),
            job("gated_push", &format!("(github.event_name == 'push') && {gate}")),
            job("gated_vars", &format!("(vars.RUN_E2E == 'true') && {gate}")),
        ]
        .concat();
        let skip_on_prs = skipped_on_every_pr(&job_ifs(&wf));
        let state = |job: &str| {
            act_skip(
                job_state(job, Some(0), true, false, &marks(&[]), false),
                job,
                &skip_on_prs,
            )
        };

        // GitHub runs these on a PR, or may, so act's skip says nothing
        for job in [
            "label",
            "label__2",
            "dispatch",
            "vars",
            "either",
            "not_tag",
            "gated_vars",
        ] {
            assert_eq!(state(job), IF_SKIP, "{job}");
            let states = BTreeMap::from([(job.to_string(), state(job))]);
            assert!(!workflow_passed(&states), "{job}: {states:?}");
        }

        // GitHub skips these on a PR too
        for job in ["push", "release", "tag", "off", "off_expr", "gated_push"] {
            assert_eq!(state(job), "skipped", "{job}");
        }

        assert_eq!(
            act_skip("success".into(), "vars", &skip_on_prs),
            "success",
            "a job act ran stands"
        );
    }

    #[test]
    fn continue_on_error_never_allows_a_timeout() {
        // AGENTS.md: a timeout is a failure, and this deadline is shipcheck's own
        let timed_out = job_state("test", None, false, false, &marks(&[]), true);
        let states = BTreeMap::from([
            ("lint".to_string(), "success".to_string()),
            ("test".to_string(), timed_out.clone()),
        ]);
        assert!(
            !workflow_passed(&states),
            "a workflow passed with a timed out job: {states:?}"
        );
        assert_eq!(
            timed_out, "timed out",
            "continue-on-error allowed a timeout"
        );
    }

    #[test]
    fn continue_on_error_never_allows_a_killed_job() {
        // A job that started and never printed its end marker did not end, so no step failed
        let killed = job_state(
            "test",
            Some(1),
            false,
            false,
            &marks(&[("test", 1, 0, false)]),
            true,
        );
        let states = BTreeMap::from([
            ("lint".to_string(), "success".to_string()),
            ("test".to_string(), killed.clone()),
        ]);
        assert!(
            !workflow_passed(&states),
            "a workflow passed with a killed job: {states:?}"
        );
        assert_eq!(killed, "killed", "continue-on-error allowed a killed job");
    }

    #[test]
    fn workflow_passes_only_when_every_job_passed_and_one_succeeded_or_was_skipped() {
        let allowed = format!("failure{ALLOWED}");
        let cases: &[(&[&str], bool)] = &[
            (&[], false),
            (&["success"], true),
            (&["success", "success"], true),
            (&["success", "skipped"], true),
            (&["success", &allowed], true),
            (&["skipped"], true),
            (&["skipped", "skipped"], true),
            (&[&allowed], false),
            (&["skipped", &allowed], true),
            (&["skipped", "failure, act ran nothing"], false),
            (&["success", "failure"], false),
            (&["success", "killed"], false),
            (&["success", "timed out"], false),
            (
                &["success", "no image for its runs-on label on this host"],
                false,
            ),
            (&["success", "not run, a job it needs did not pass"], false),
            (&["success", "not run, its needs form a cycle"], false),
            (&["success", "failure, shipcheck could not run it"], false),
        ];

        for (jobs, want) in cases {
            let states = jobs
                .iter()
                .enumerate()
                .map(|(n, s)| (format!("j{n}"), s.to_string()))
                .collect();
            assert_eq!(workflow_passed(&states), *want, "{jobs:?}");
        }
    }

    #[test]
    fn plan_reads_the_config_and_the_workflows_from_main() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let ci = format!("{WORKFLOWS}/ci.yml");
        let main_config = "runner = \"host\"\n[host]\nchecks = { test = \"true\" }\n";
        let main_ci =
            "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";
        sh(repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(repo.join(WORKFLOWS)).unwrap();
        fs::write(repo.join(CONFIG), main_config).unwrap();
        fs::write(repo.join(&ci), main_ci).unwrap();
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "main"]);

        // The PR edits both files and adds a workflow, and the working copy edits them again
        sh(repo, &["switch", "-q", "-c", "feature"]);
        fs::write(
            repo.join(CONFIG),
            main_config.replace("\"true\"", "\"exit 0\""),
        )
        .unwrap();
        fs::write(repo.join(&ci), main_ci.replace("run: true", "run: exit 0")).unwrap();
        fs::write(repo.join(WORKFLOWS).join("new.yml"), main_ci).unwrap();
        sh(repo, &["add", "-A"]);
        sh(repo, &["commit", "-qm", "feature"]);
        fs::write(repo.join(CONFIG), "runner = \"act\"\n").unwrap();
        fs::write(repo.join(&ci), "on: push\n").unwrap();

        let plan = plan(repo, "HEAD", "main").unwrap();
        assert_eq!(plan.sha, sh(repo, &["rev-parse", "feature"]));
        assert_eq!(String::from_utf8_lossy(&plan.config_text), main_config);
        assert_eq!(plan.config.host.checks["test"], "true");
        assert!(!plan.defaults);
        assert_eq!(
            plan.workflows,
            BTreeMap::from([(ci.clone(), main_ci.as_bytes().to_vec())])
        );
    }
}
