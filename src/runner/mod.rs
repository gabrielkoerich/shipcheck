use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use crate::config::RepoConfig;

pub mod act;
pub mod colima;
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
    /// act said the job's `if:` skipped it
    pub skipped: bool,
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

/// The runners a config uses: act for its workflow jobs, host for its `[host]` checks, or both
pub struct Runners {
    pub act: Option<Box<dyn Runner>>,
    pub host: Option<Box<dyn Runner>>,
}

impl Runners {
    /// `repo` is the checkout the run reads git from, where host checks run their `versions`
    pub fn new(config: &RepoConfig, repo: &Path) -> Self {
        Runners {
            act: config.acts().then(|| {
                Box::new(
                    act::Act::new(config.act.clone())
                        .with_network(config.network)
                        .with_shards(config.shard_tests)
                        .with_engines(config.engines()),
                ) as Box<dyn Runner>
            }),
            host: config
                .hosts()
                .then(|| Box::new(host::Host::new(config.host.clone(), repo)) as Box<dyn Runner>),
        }
    }

    pub fn all(&self) -> impl Iterator<Item = &dyn Runner> {
        self.act.iter().chain(&self.host).map(|r| r.as_ref())
    }

    /// The host runner for [`host::HOST`]'s checks, act for a workflow's jobs
    pub fn for_file(&self, file: &str) -> Result<&dyn Runner, String> {
        let runner = if file == host::HOST {
            &self.host
        } else {
            &self.act
        };
        runner
            .as_deref()
            .ok_or_else(|| format!("no runner for {file}"))
    }

    /// Each runner's identity, act's first, as the cache key holds it
    pub fn identity(&self) -> Result<String, String> {
        let ids: Vec<String> = self.all().map(|r| r.identity()).collect::<Result<_, _>>()?;
        Ok(ids.join("\n"))
    }

    /// The kind the cache key holds: `act` or `host` alone, `mixed` for both
    pub fn kind(&self) -> &'static str {
        match (&self.act, &self.host) {
            (Some(_), Some(_)) => "mixed",
            (None, Some(_)) => "host",
            _ => "act",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);

    impl Runner for Named {
        fn identity(&self) -> Result<String, String> {
            Ok(self.0.to_string())
        }

        fn prepare(&self, workflow: &str, _file: &str) -> Result<String, String> {
            Ok(workflow.to_string())
        }

        fn run(&self, _request: &RunRequest, _log: &Path) -> Result<Outcome, String> {
            Err("not run".into())
        }

        fn reclaim(&self, _run_dir: &Path) {}

        fn clean(&self, _repo_id: Option<&str>) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn a_config_without_jobs_keys_its_one_runner_as_before() {
        let kind = |text: &str| {
            let config = RepoConfig::parse(text).unwrap();
            let runners = Runners::new(&config, Path::new("/"));
            (runners.kind(), runners.all().count())
        };
        assert_eq!(kind("workflows = [\"ci.yml\"]"), ("act", 1));
        assert_eq!(
            kind("runner = \"host\"\n[host.checks]\nt = \"x\""),
            ("host", 1)
        );
        let mixed =
            "runner = \"host\"\n[host.checks]\nt = \"x\"\n[jobs.\"ci.yml/e2e\"]\nrunner = \"act\"";
        assert_eq!(kind(mixed), ("mixed", 2));
        let config = RepoConfig::parse(mixed).unwrap();
        let mut runners = Runners::new(&config, Path::new("/"));
        // A stand-in for act, whose binary the test job may lack
        runners.act = Some(Box::new(Named("act version 0.2.89")));
        assert_eq!(runners.identity().unwrap(), "act version 0.2.89\nhost");

        assert!(runners.for_file(host::HOST).is_ok() && runners.for_file("ci.yml").is_ok());
        let host = RepoConfig::parse("runner = \"host\"\n[host.checks]\nt = \"x\"").unwrap();
        assert!(
            Runners::new(&host, Path::new("/"))
                .for_file("ci.yml")
                .is_err()
        );
    }
}
