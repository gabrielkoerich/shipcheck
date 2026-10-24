use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use crate::config::{RepoConfig, RunnerKind};

pub mod act;
pub mod gc;
pub mod host;
pub mod shard;

/// What executes a workflow. The cache, checkouts, markers, hooks and checks are shared
pub trait Runner: Sync {
    /// Name and version, part of the cache key
    fn identity(&self) -> Result<String, String>;

    /// Refuses to run on a host whose isolation is known to be broken. Called once per run
    fn preflight(&self) -> Result<(), String> {
        Ok(())
    }

    /// Rewrites one workflow copy into what this runner can run. Pure, its output goes into the key
    fn prepare(&self, workflow: &str, file: &str) -> Result<String, String>;

    /// Runs one job of a prepared workflow in its own checkout, writes its log, and waits within the
    /// timeout. Jobs it needs have already passed, their outputs are in the request
    fn run(&self, request: &RunRequest, log: &Path) -> Result<Outcome, String>;

    /// Removes what a dead run left in this job's checkout. Called with the slot's lock held
    fn reclaim(&self, run_dir: &Path);

    /// Removes this runner's caches for one repository, or for all of them
    fn clean(&self, repo_id: Option<&str>) -> Vec<String>;

    /// Removes this runner's state that no run used for a while. Called at the end of a run
    fn prune(&self) -> Vec<String> {
        Vec::new()
    }
}

pub struct RunRequest<'a> {
    /// The job's own checkout
    pub run_dir: &'a Path,
    pub file: &'a str,
    /// The prepared workflow, outside the checkout, so the checkout's own files stay as the base has them
    pub workflow: &'a Path,
    pub job: &'a str,
    /// Outputs of each job in `needs`, by job id
    pub needs: &'a BTreeMap<String, BTreeMap<String, String>>,
    /// Shared by the jobs of one workflow run, so artifacts pass between them
    pub artifacts: &'a Path,
    pub repo_id: &'a str,
    pub timeout: Duration,
    /// Run the job's pytest step in shards, if the prepared workflow has one
    pub shard: bool,
    /// The tree is the base's, so what the job leaves in shared caches may reach runs of the base
    pub on_base: bool,
}

pub struct Outcome {
    /// None when the run timed out
    pub exit: Option<i32>,
    /// Jobs this host cannot run, which fail the workflow
    pub unsupported_jobs: Vec<String>,
    /// The job's `outputs`, for the jobs that need it
    pub outputs: BTreeMap<String, String>,
    /// How many shards the job's pytest step ran in, when it ran sharded
    pub shards: Option<u32>,
    /// What the job's steps wrote to `$GITHUB_STEP_SUMMARY`, in step order, cut to [`MAX_SUMMARY`]
    pub summary: String,
}

/// The most of one job's step summary that is kept, in bytes
pub const MAX_SUMMARY: usize = 20_000;

/// The first `MAX_SUMMARY` bytes of a summary, with a note when it was longer
pub fn cap_summary(text: &str) -> String {
    let text = text.trim_end();

    if text.len() <= MAX_SUMMARY {
        return text.to_string();
    }

    let mut cut = MAX_SUMMARY;

    while !text.is_char_boundary(cut) {
        cut -= 1;
    }

    format!("{}\n\n[...cut at {MAX_SUMMARY} bytes...]", &text[..cut])
}

/// `repo` is the checkout the run reads git from, where host mode runs its `versions`
pub fn from_config(config: &RepoConfig, repo: &Path) -> Box<dyn Runner> {
    match config.runner {
        RunnerKind::Host => Box::new(host::Host::new(config.host.clone(), repo)),
        RunnerKind::Act => Box::new(
            act::Act::new(config.act.clone())
                .with_network(config.network)
                .with_shards(config.shard_tests),
        ),
    }
}
