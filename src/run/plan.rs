use super::{CONFIG, Jobs, Plan, select};
use crate::cache::{self, KeyInputs};
use crate::config::{RepoConfig, RunnerKind};
use crate::git::{git, git_raw};
use crate::paths::home;
use crate::runner::act;
use crate::runner::host::{self as host_runner, HOST};
use crate::runner::{Runner, Runners};
use crate::workflow::WORKFLOWS;
use crate::{run::trust, workflow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
pub(super) fn index_paths(repo: &Path) -> Result<(PathBuf, PathBuf), String> {
    let index = PathBuf::from(git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?);
    let temp = index.with_file_name(format!("shipcheck-index-{}", std::process::id()));
    Ok((index, temp))
}

pub(super) fn copy_index(index: &Path, temp: &Path) -> Result<(), String> {
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

pub(super) fn snapshot_tree(
    repo: &Path,
    index: &Path,
    temp: &Path,
    tree: Tree,
) -> Result<String, String> {
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

pub(super) fn tested_file(repo: &Path) -> PathBuf {
    let id = cache::hex(&Sha256::digest(repo.to_string_lossy().as_bytes()));
    home().join("tested").join(format!("{}.json", &id[..16]))
}

pub fn record_tested(repo: &Path, tested: &Tested) -> Result<(), String> {
    let file = tested_file(repo);
    fs::create_dir_all(file.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
    let text = serde_json::to_string(tested).map_err(|e| e.to_string())?;
    fs::write(&file, text).map_err(|e| e.to_string())
}

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

pub(super) fn ci_changes(repo: &Path, base: &str, sha: &str) -> Result<Vec<String>, String> {
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
pub(super) fn check_definition_changes(
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

pub(super) fn act_jobs(config: &RepoConfig, graphs: &Graphs) -> Result<Jobs, String> {
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
pub(super) type Graphs = BTreeMap<String, Vec<(String, Vec<String>)>>;

/// Whether `job` names job `id`, itself or as one leg `<job>__<n>` of its matrix
pub(super) fn names_job(id: &str, job: &str) -> bool {
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
pub(super) fn as_run_body(
    runner: &dyn Runner,
    body: &[u8],
    file: &str,
    nonce: &str,
) -> Result<Vec<u8>, String> {
    let prepared = runner.prepare(&String::from_utf8_lossy(body), file)?;
    Ok(workflow::add_markers(&prepared, nonce)?.into_bytes())
}

pub(super) fn file_name(path: &str) -> String {
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
pub(super) fn env_skips(value: Option<&str>) -> bool {
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
