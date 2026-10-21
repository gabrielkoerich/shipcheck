use std::collections::{BTreeMap, BTreeSet};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub workflows: Vec<String>,
    #[serde(default = "on_push")]
    pub on: Vec<Trigger>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "thirty")]
    pub timeout_minutes: u64,
    #[serde(default = "one")]
    pub retries: u32,
    /// The most jobs one run starts at once, half the CPUs when unset
    #[serde(default)]
    pub max_parallel: Option<usize>,
    #[serde(default)]
    pub runner: RunnerKind,
    /// What jobs can reach. None by default, since job code is a branch's code on this machine
    #[serde(default)]
    pub network: Network,
    /// Whether failed jobs' log tails go into the check, which is public on a public repository
    #[serde(default)]
    pub logs: Logs,
    #[serde(default)]
    pub act: Act,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum Logs {
    #[default]
    None,
    Failed,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunnerKind {
    #[default]
    Act,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// No network at all: not the host, the LAN or the internet
    #[default]
    None,
    /// The runner's own network, which for act today can still reach the host (docs/SECURITY.md)
    Internet,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Push,
    Commit,
}

fn on_push() -> Vec<Trigger> {
    vec![Trigger::Push]
}

fn thirty() -> u64 {
    30
}

fn one() -> u32 {
    1
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Act {
    pub platforms: BTreeMap<String, String>,
    /// An image for one job, as `"<workflow file>/<job id>"`, when jobs sharing a label need different ones
    pub jobs: BTreeMap<String, String>,
    pub cache_paths: Vec<String>,
    /// A directory of its own under JOB_HOMES for each job, per environment variable
    pub job_homes: BTreeMap<String, Home>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum Home {
    Dir(String),
    Full { dir: String, path: Option<String> },
}

impl Home {
    pub fn dir(&self) -> &str {
        match self {
            Home::Dir(dir) | Home::Full { dir, .. } => dir,
        }
    }

    /// A subdirectory added to the job's `PATH`
    pub fn path(&self) -> Option<&str> {
        match self {
            Home::Dir(_) => None,
            Home::Full { path, .. } => path.as_deref(),
        }
    }
}

impl Default for Act {
    fn default() -> Self {
        Act {
            platforms: BTreeMap::new(),
            jobs: BTreeMap::new(),
            // Parallel jobs share these volumes. Tools that break under sharing get a home per job in JOB_HOMES
            cache_paths: [
                crate::runner::act::JOB_HOMES,
                "/root/.npm",
                "/root/.bun/install/cache",
                "/root/.cache",
            ]
            .map(String::from)
            .to_vec(),
            // A shared ~/.rustup and ~/.cargo broke each other in the oblivion trial, and act's one
            // tool cache for every job gave parallel setup actions half-copied Zig and JDK installs
            job_homes: BTreeMap::from([
                ("RUSTUP_HOME".to_string(), Home::Dir("rustup".into())),
                (
                    "RUNNER_TOOL_CACHE".to_string(),
                    Home::Dir("toolcache".into()),
                ),
                (
                    "CARGO_HOME".to_string(),
                    Home::Full {
                        dir: "cargo".into(),
                        path: Some("bin".into()),
                    },
                ),
            ]),
        }
    }
}

impl Act {
    /// Job id to image for the jobs of one workflow file that `[act.jobs]` names
    pub fn job_images(&self, file: &str) -> BTreeMap<String, String> {
        self.jobs
            .iter()
            .filter_map(|(name, image)| {
                let (f, job) = name.split_once('/')?;
                (f == file && !job.is_empty()).then(|| (job.to_string(), image.clone()))
            })
            .collect()
    }
}

impl RepoConfig {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("shipcheck.toml: {e}"))
    }

    pub fn max_parallel(&self) -> usize {
        self.max_parallel
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() / 2))
            .max(1)
    }

    pub fn runs_on(&self, trigger: Trigger) -> bool {
        self.on.contains(&trigger)
    }

    /// The `paths` filter, or None when every change counts
    pub fn path_filter(&self) -> Result<Option<GlobSet>, String> {
        if self.paths.is_empty() {
            return Ok(None);
        }

        let mut set = GlobSetBuilder::new();

        for pattern in &self.paths {
            set.add(Glob::new(pattern).map_err(|e| format!("paths: {e}"))?);
        }

        set.build().map(Some).map_err(|e| format!("paths: {e}"))
    }

    pub fn validate(&self, workflows: &BTreeSet<String>) -> Result<(), String> {
        let mut problems = Vec::new();

        if self.workflows.is_empty() {
            problems.push("workflows is empty, so nothing would run".to_string());
        }

        let mut seen = BTreeSet::new();

        for file in &self.workflows {
            if !workflows.contains(file) {
                problems.push(format!("workflow {file} does not exist"));
            }

            // A duplicate would run the same workflow twice
            if !seen.insert(file) {
                problems.push(format!("workflow {file} is listed twice"));
            }
        }

        if self.max_parallel == Some(0) {
            problems.push("max_parallel is 0, so no job would start".to_string());
        }

        if let Err(e) = self.path_filter() {
            problems.push(e);
        }

        for (label, image) in self.act.platforms.iter().chain(&self.act.jobs) {
            if image == "-self-hosted" {
                // A branch's job code would run as the developer, with every file and the App key
                problems.push(format!(
                    "platform {label} maps to -self-hosted, which runs jobs on this machine outside a container. shipcheck never does that"
                ));
            } else if !image.contains("@sha256:") {
                problems.push(format!("platform {label} is not pinned by digest: {image}"));
            }
        }

        for name in self.act.jobs.keys() {
            let ok = name
                .split_once('/')
                .is_some_and(|(f, job)| self.workflows.contains(&f.to_string()) && !job.is_empty());

            if !ok {
                problems.push(format!(
                    "act.jobs entry {name:?} must be \"<a file in workflows>/<job id>\""
                ));
            }
        }

        // Each path becomes part of a space-separated docker option, so nothing in it may split it
        for path in &self.act.cache_paths {
            let ok = path.starts_with('/')
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c));

            if !ok {
                problems.push(format!(
                    "cache path {path:?} must be absolute and use only letters, digits and ._/-"
                ));
            }
        }

        // Names and directories go into a shell step and into paths
        let word = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        };

        for (var, home) in &self.act.job_homes {
            let name_ok = var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && var.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_');
            let dirs_ok = std::iter::once(home.dir())
                .chain(home.path())
                .all(|d| d.split('/').all(|part| word(part) && part != ".."));

            if !name_ok || !dirs_ok {
                problems.push(format!(
                    "act.job_homes entry {var:?} needs a variable name and relative paths of letters, digits and ._-"
                ));
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflows(files: &[&str]) -> BTreeSet<String> {
        files.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn parses_defaults() {
        let cfg = RepoConfig::parse(r#"workflows = ["ci.yml"]"#).unwrap();
        assert_eq!(cfg.workflows, ["ci.yml"]);
        assert_eq!(cfg.on, [Trigger::Push]);
        assert!(cfg.paths.is_empty());
        assert_eq!(cfg.timeout_minutes, 30);
        assert_eq!(cfg.retries, 1);
        assert!(cfg.max_parallel() >= 1);
        assert!(cfg.path_filter().unwrap().is_none());
    }

    #[test]
    fn parses_the_runner() {
        let default = RepoConfig::parse(r#"workflows = ["ci.yml"]"#).unwrap();
        assert_eq!(default.runner, RunnerKind::Act);
        let act = RepoConfig::parse("workflows = [\"ci.yml\"]\nrunner = \"act\"").unwrap();
        assert_eq!(act.runner, RunnerKind::Act);
        assert!(RepoConfig::parse("workflows = [\"ci.yml\"]\nrunner = \"other\"").is_err());
    }

    #[test]
    fn parses_logs() {
        let default = RepoConfig::parse(r#"workflows = ["ci.yml"]"#).unwrap();
        assert_eq!(default.logs, Logs::None);
        let failed = RepoConfig::parse("workflows = [\"ci.yml\"]\nlogs = \"failed\"").unwrap();
        assert_eq!(failed.logs, Logs::Failed);
        assert!(RepoConfig::parse("workflows = [\"ci.yml\"]\nlogs = \"all\"").is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(RepoConfig::parse("workflows = []\nskip_tests = true").is_err());
        assert!(RepoConfig::parse("workflows = []\non = [\"merge\"]").is_err());
    }

    #[test]
    fn matches_paths_like_a_workflow_filter() {
        let cfg = RepoConfig::parse(
            r#"
            workflows = ["ci.yml"]
            on = ["push", "commit"]
            paths = ["src/**", "Cargo.*"]
            "#,
        )
        .unwrap();
        assert!(cfg.runs_on(Trigger::Commit));
        let filter = cfg.path_filter().unwrap().unwrap();
        assert!(filter.is_match("src/deep/lib.rs"));
        assert!(filter.is_match("Cargo.lock"));
        assert!(!filter.is_match("docs/README.md"));
    }

    #[test]
    fn accepts_a_complete_config() {
        let cfg = RepoConfig::parse(
            r#"
            workflows = ["ci.yml"]
            timeout_minutes = 10
            [act]
            platforms = { "ubuntu-latest" = "img@sha256:abc" }
            "#,
        )
        .unwrap();
        let wf = workflows(&["ci.yml", "deploy.yml"]);
        assert_eq!(cfg.validate(&wf), Ok(()));
    }

    #[test]
    fn rejects_empty_and_duplicate_workflows() {
        let wf = workflows(&["ci.yml"]);
        let empty = RepoConfig::parse("workflows = []").unwrap();
        assert!(
            empty
                .validate(&wf)
                .unwrap_err()
                .contains("workflows is empty")
        );
        let twice = RepoConfig::parse(r#"workflows = ["ci.yml", "ci.yml"]"#).unwrap();
        assert!(
            twice
                .validate(&wf)
                .unwrap_err()
                .contains("ci.yml is listed twice")
        );
    }

    #[test]
    fn checks_act_jobs_keys() {
        let cfg = RepoConfig::parse(
            r#"
            workflows = ["ci.yml"]
            [act]
            jobs = { "ci.yml/build" = "img@sha256:a", "other.yml/x" = "img@sha256:b", "nojob" = "img@sha256:c", "ci.yml/" = "img@sha256:d" }
            "#,
        )
        .unwrap();
        let err = cfg
            .validate(&workflows(&["ci.yml", "other.yml"]))
            .unwrap_err();
        assert!(!err.contains("\"ci.yml/build\""));
        assert!(
            err.contains("\"other.yml/x\""),
            "a workflow not in workflows"
        );
        assert!(err.contains("\"nojob\""));
        assert!(err.contains("\"ci.yml/\""));
        assert_eq!(cfg.act.job_images("ci.yml").len(), 1);
        assert!(cfg.act.job_images("missing.yml").is_empty());
    }

    #[test]
    fn rejects_negative_or_wrong_typed_numbers() {
        assert!(RepoConfig::parse("workflows = [\"ci.yml\"]\ntimeout_minutes = -1").is_err());
        assert!(RepoConfig::parse("workflows = [\"ci.yml\"]\nretries = \"one\"").is_err());
        assert!(RepoConfig::parse("workflows = \"ci.yml\"").is_err());
    }

    #[test]
    fn reports_every_problem() {
        let cfg = RepoConfig::parse(
            r#"
            workflows = ["ci.yml", "gone.yml"]
            max_parallel = 0
            paths = ["src/[unclosed"]
            [act]
            platforms = { "ubuntu-latest" = "img:latest", "macos-latest" = "-self-hosted" }
            cache_paths = ["/root/.cargo", "/x --privileged", "relative"]
            job_homes = { GOMODCACHE = "gomod", "A;B" = "x", X = "../up", Y = { dir = "y", path = "b in" } }
            "#,
        )
        .unwrap();
        let wf = workflows(&["ci.yml", "deploy.yml"]);
        let err = cfg.validate(&wf).unwrap_err();
        assert!(err.contains("gone.yml does not exist"));
        assert!(err.contains("paths:"));
        assert!(err.contains("max_parallel is 0"));
        assert!(err.contains("ubuntu-latest is not pinned by digest"));
        assert!(err.contains("macos-latest maps to -self-hosted"));
        assert!(err.contains("\"/x --privileged\" must be absolute"));
        assert!(err.contains("\"relative\" must be absolute"));
        assert!(!err.contains("/root/.cargo"));
        assert!(!err.contains("GOMODCACHE"));

        for bad in ["\"A;B\"", "\"X\"", "\"Y\""] {
            assert!(err.contains(&format!("job_homes entry {bad}")), "{bad}");
        }
    }
}
