use crate::base::remote_base_sha;
use crate::clock::{clock, timing, unix_now};
use crate::config::{Check, RepoConfig};
use crate::git::{git, repo_id};
use crate::paths::{git_lock_in, home, log_dir, partial_log_dir, secrets_file};
use crate::runner::{act, host};
use crate::ui::printable;
use crate::workflow::{WORKFLOWS, false_on_every_pr};
use crate::{secret, workflow};
use checkout::{checkout, empty_dir, remove_planted_links, standalone_git};
use plan::{Graphs, act_jobs, as_run_body, file_name, names_job};
use reuse::{Previous, Reuse, link_branch, previous};
use slots::{claim_runner, claim_slot, nonce};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache;
use crate::runner::host::{HOST, runner_of};
use crate::runner::shard::{self, Rejected, ShardRun};
use crate::runner::{RunRequest, Runners};

mod clean;
pub use clean::{Scope, clean, prune};

mod checkout;

mod slots;
pub use slots::{find_key, lock_key, lock_tree, max_runs, reusable, running};

mod reuse;
pub use reuse::{cached, fail_closed, holds, last_run, lookup, skipped};

mod plan;
pub use plan::{
    CALLER_PATH, Failed, Tested, Tree, commit, expected_jobs, jobs, last_tested, message_skips,
    on_default_branch, plan, record_tested, skip_requested, snapshot, untracked,
};

pub mod queue;
pub mod templates;
pub mod trust;

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

/**
 * The working copy, or with `Staged` the index, as a commit no ref names, with HEAD as parent, like
 * `git stash create`. The working copy is HEAD's tree with every edit and every file that is not
 * gitignored. It is built in a copy of the index, so the user's index, branches and stash stay as
 * they are. None when the tree is HEAD's
 **/
/**
 * git rehashes an entry whose file changed no earlier than the index was written, since its stats
 * cannot tell an edit in that second apart. A copy with a new time loses that, and an edit of the
 * same size in the same second as `git add` went unseen. Linux's copy takes the time of the copy
 **/
/**
 * The remote's default branch as `origin/<branch>`, None when the remote did not answer. Asked once
 * per process for each checkout and origin URL. Never the local origin/HEAD, which anyone with the
 * clone can repoint at a branch whose shipcheck.toml and workflows would then run
 **/
/**
 * The default branch, `base` without `origin/`, when `commit` (no `sha`) runs with HEAD on it, or
 * when `post` names its tip, locally or on `base`. Both refuse there, PRs carry the checks
 **/
/**
 * Commits exactly the tree the last run tested, through `git commit`, so hooks, signing and the
 * author work as usual. The commit uses a copy of the index built by the snapshot's rules, and
 * the tree is compared on that copy, so what is compared is what is committed. The copy becomes
 * the real index only after the commit. On any refusal the real index is untouched
 **/
/**
 * What the commit changes in its workflows and `shipcheck.toml` since it left the base. These never
 * run, but once merged they change CI for everyone, so the check names them, and the jobs
 * a change lets fail or never runs. Also the files that define what a command does, such as
 * package.json scripts, which do run from the commit in either mode
 **/
/**
 * The act jobs of `config` per workflow file: each listed workflow whole, each act `[jobs]` entry,
 * and every job they need, less the jobs host entries take out. A host entry must name a job of its
 * file and a host check of that id, so no job is dropped without a check in its place
 **/
/**
 * The key and base commit of the last recorded run of `tree` whose key `has` something. `logs` and
 * `status` read it when the key under origin/main has nothing, such as after a trial with
 * `--base <branch>`, which keys the run by that branch's shipcheck.toml
 **/
/**
 * What a check shows of a stored result must hold before it is posted again, or the result does not
 * count and the commit runs: the plan's tree, a base on the plan's base, and each log hashing to its
 * digest. The key holds the tree, still the stored file is checked, never trusted
 **/
/**
 * This tree's own failed result, or with `Reuse::Branch` the branch's last one when it failed and
 * only its tree differs, the same base config, commands, workflows, versions and images. Its passes
 * are kept
 **/
/**
 * A pass with nothing run when no file changed since the merge base with `base` matches `paths`,
 * like a path filter on a workflow. It depends on where `base` is, which the cache key does not
 * hold, so it is worked out again each time instead of cached. Any error means run
 **/
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

/**
 * Builds embed absolute paths, such as cargo's `CARGO_BIN_EXE_*`, and a warm `target/` cloned to a
 * new path still points at the old one. So runs of a repo reuse a few fixed paths, one per slot,
 * held by a lock for the length of a run. The directory itself is still new for every run
 **/
/**
 * Takes the tree's lock, and when another run held it, the time this one began to wait. A result
 * stored after that is the one this run waited for, see [`reusable`]
 **/
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

/**
 * The live tickets ahead of `ticket`, removing those of runs that died. Only the head takes a slot,
 * so slots go out in the queue's order
 **/
/**
 * Waits in the machine's queue for a run slot. `shipcheck run`, which an agent waits on, goes ahead
 * of the runs the hooks and the daemon start, and each kind goes in the order it came
 **/
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
pub const UNTRUSTED: &str = "this repository asks to run it with sandbox = false, so it runs in a sandbox here. Run `shipcheck trust` to allow it if you trust the repository";

/// What a run says when its host checks run in the sandbox shipcheck runs in
pub const INSIDE: &str = "host checks run in the sandbox shipcheck runs in, not one of their own, since macOS starts no sandbox inside another, and get what it allows";

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

    if host::loose_sandbox() {
        say(&format!("{HOST}: {INSIDE}"));
    }

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

/**
 * The clone keeps the template's modification times, and git rewrites only the files that differ
 * from the template's base. The template's build output can come from another tree and another
 * slot, so cargo or make would trust it for a file this tree has unchanged. Every tracked file is
 * made newer than any build output, so each run builds its own tree in its own slot
 **/
/**
 * Proves the job's files are the tested tree: every tracked file on disk is hashed again into a new
 * index, which has no stat cache to skip a file with, and the tree written from it must be the
 * plan's. A file edited, deleted, relinked or with another mode changes that tree. Ignored and
 * untracked files, such as warm build output, are not part of a tree, so they never count
 **/
/**
 * Docker Desktop's file sharing can keep a bind mount on a directory that was deleted and made again
 * at the same path, so a job saw an empty checkout and could not exec. A job's checkout directory is
 * made once and only emptied between runs
 **/
/**
 * A job checkout is a git worktree, whose `.git` file points into the repository's own `.git` on the
 * host, which the job cannot see and must not read: it holds unpushed branches, the stash and remote
 * credentials. So the checkout gets a new repository with only what the tested commit and the tags
 * reach, fetched from the worktree itself, whose `HEAD` is that commit. A job may change it freely.
 * Returns the worktree's `.git` file, which [`finish_job`] puts back
 **/
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

/**
 * Removes what shipcheck keeps for one repository: its Docker volumes, warm template and run slots,
 * or for every repository with `All`, which also clears results, logs and records. Everything but
 * the records rebuilds on the next run, which is only slower. Refuses while a run holds a slot
 **/
#[cfg(test)]
mod tests;
