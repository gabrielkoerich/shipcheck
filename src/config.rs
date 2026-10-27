use std::collections::{BTreeMap, BTreeSet};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// Empty in host mode, which runs `[host]` checks instead
    #[serde(default)]
    pub workflows: Vec<String>,
    #[serde(default = "on_push")]
    pub on: Vec<Trigger>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "thirty")]
    pub timeout_minutes: u64,
    #[serde(default = "one")]
    pub retries: u32,
    /// The most jobs one run starts at once. Unset: host CPUs minus 1, capped by Docker's CPUs and memory
    #[serde(default)]
    pub max_parallel: Option<usize>,
    #[serde(default)]
    pub runner: RunnerKind,
    /// What jobs can reach. None by default, since job code is a branch's code on this machine
    #[serde(default)]
    pub network: Network,
    /// Which jobs' log tails go into their checks, which are public on a public repository
    #[serde(default)]
    pub logs: Logs,
    /// Splits a pytest step into parallel shards in the job's container, off unless set
    #[serde(default)]
    pub shard_tests: ShardTests,
    #[serde(default)]
    pub act: Act,
    #[serde(default)]
    pub host: Host,
    /// One job's runner and engine, as `"<workflow file>/<job id>"`, when it differs from `runner`
    #[serde(default)]
    pub jobs: BTreeMap<String, Job>,
    /// Whether the daemon reruns and posts this repository's PRs that changed on GitHub
    #[serde(default = "yes")]
    pub daemon: bool,
    /// GitHub logins besides the daemon's own whose PRs it may run, for the owner to choose
    #[serde(default)]
    pub daemon_authors: Vec<String>,
    #[serde(default)]
    pub team: Team,
}

/// What a team on a paid plan sends to the service, all off by default. Only `records` does anything yet
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Team {
    pub records: bool,
    pub cache: bool,
    pub logs: bool,
}

/// The checks of `runner = "host"`, each a shell command run natively in its own clone
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Host {
    pub checks: BTreeMap<String, Check>,
    /// Commands whose output goes into the cache key, so a toolchain change runs again
    pub versions: Vec<String>,
    /// Names of variables passed from the user's environment, beyond the base list
    pub env: Vec<String>,
}

/// A host check: its command, or a table with `run` and `sandbox = false` to run it outside the OS sandbox
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub run: String,
    pub sandbox: bool,
    /// Set by the plan when the base asks for `sandbox = false` and this machine does not trust the repository
    pub untrusted: bool,
}

impl<'de> Deserialize<'de> for Check {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            run: String,
            #[serde(default = "yes")]
            sandbox: bool,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Command(String),
            Table(Table),
        }

        Ok(match Either::deserialize(d)? {
            Either::Command(run) => Check::from(run),
            Either::Table(t) => Check {
                run: t.run,
                sandbox: t.sandbox,
                untrusted: false,
            },
        })
    }
}

impl std::ops::Deref for Check {
    type Target = str;

    fn deref(&self) -> &str {
        &self.run
    }
}

impl PartialEq<&str> for Check {
    fn eq(&self, other: &&str) -> bool {
        self.run == *other
    }
}

impl From<&str> for Check {
    fn from(run: &str) -> Self {
        Check {
            run: run.into(),
            sandbox: true,
            untrusted: false,
        }
    }
}

impl From<String> for Check {
    fn from(run: String) -> Self {
        Check {
            run,
            sandbox: true,
            untrusted: false,
        }
    }
}

/// `shard_tests = true` for as many shards as the container has CPUs, at most 6, or a fixed number
#[derive(Debug, Deserialize, PartialEq, Clone, Copy)]
#[serde(untagged)]
pub enum ShardTests {
    On(bool),
    Fixed(u32),
}

impl Default for ShardTests {
    fn default() -> Self {
        ShardTests::On(false)
    }
}

impl ShardTests {
    pub fn enabled(self) -> bool {
        !matches!(self, ShardTests::On(false))
    }
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum Logs {
    Failed,
    #[default]
    All,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunnerKind {
    #[default]
    Act,
    Host,
}

/// A `[jobs]` entry. `runner` unset is the repository's `runner`
#[derive(Debug, Deserialize, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub runner: Option<RunnerKind>,
    /// What an act job runs on, Docker unless set
    pub engine: Option<Engine>,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[default]
    Docker,
    /// A Colima VM per job, which reaches this machine's loopback, for trusted repositories only
    Colima,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// No network at all: not the host, the LAN or the internet
    #[default]
    None,
    /// A Docker bridge: the internet and the bridge gateway, not the host loopback
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

fn yes() -> bool {
    true
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
    /// Paths with a volume per repository, slot and job, for caches that do not lock themselves
    pub slot_paths: Vec<String>,
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
                "/home/runner/.npm",
                "/home/runner/.bun/install/cache",
                "/home/runner/.cache",
            ]
            .map(String::from)
            .to_vec(),
            // apt fails on a held lock instead of waiting
            slot_paths: vec!["/var/lib/apt/lists".to_string()],
            // A shared ~/.rustup and ~/.cargo broke each other in an Anchor project's trial, and act's one
            // tool cache for every job gave parallel setup actions half-copied Zig, JDK and Python installs
            job_homes: BTreeMap::from([
                ("RUSTUP_HOME".to_string(), Home::Dir("rustup".into())),
                (
                    "RUNNER_TOOL_CACHE".to_string(),
                    Home::Dir("toolcache".into()),
                ),
                // setup-python's installer prefers it to RUNNER_TOOL_CACHE, and the image sets it
                (
                    "AGENT_TOOLSDIRECTORY".to_string(),
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

/// The caches of one language, kept only for a tree with one of its files
struct Ecosystem {
    /// File names anywhere in the tree, or `*.ext` for an extension
    files: &'static [&'static str],
    cache_paths: &'static [&'static str],
    slot_paths: &'static [&'static str],
    job_homes: &'static [(&'static str, &'static str)],
}

const JS_LOCKS: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];

/**
 * Shared volumes only for caches that lock themselves, slot volumes and per-job homes for the
 * rest. Maven locks only inside one process, Gradle asks for a
 * lock by a localhost ping that never reaches another container, yarn v1 has no lock, nub's is
 * unchecked, and Composer, Bundler, Hex and rebar3 write in place or under a PID temp name
 **/
const ECOSYSTEMS: &[Ecosystem] = &[
    Ecosystem {
        files: JS_LOCKS,
        cache_paths: &[],
        // setup-nub installs from any of these lockfiles
        slot_paths: &["/home/runner/.local/share/nub"],
        job_homes: &[],
    },
    Ecosystem {
        files: &["yarn.lock"],
        cache_paths: &["/home/runner/.yarn/berry/cache"],
        slot_paths: &["/home/runner/.cache/yarn"],
        job_homes: &[],
    },
    Ecosystem {
        files: &["pnpm-lock.yaml"],
        cache_paths: &[],
        slot_paths: &[],
        job_homes: &[("npm_config_store_dir", "pnpm")],
    },
    Ecosystem {
        files: &["go.sum", "go.mod"],
        cache_paths: &["/home/runner/go/pkg/mod"],
        slot_paths: &[],
        job_homes: &[],
    },
    Ecosystem {
        files: &["pom.xml"],
        cache_paths: &[],
        slot_paths: &["/home/runner/.m2"],
        job_homes: &[],
    },
    Ecosystem {
        files: &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        cache_paths: &[],
        slot_paths: &["/home/runner/.gradle"],
        job_homes: &[],
    },
    Ecosystem {
        files: &["build.sbt"],
        cache_paths: &[],
        slot_paths: &["/home/runner/.sbt"],
        job_homes: &[],
    },
    Ecosystem {
        files: &["Gemfile.lock", "Gemfile", "*.gemspec"],
        cache_paths: &[],
        slot_paths: &[],
        job_homes: &[("BUNDLE_USER_CACHE", "bundle")],
    },
    Ecosystem {
        files: &["composer.lock", "composer.json"],
        cache_paths: &[],
        slot_paths: &[],
        job_homes: &[("COMPOSER_CACHE_DIR", "composer")],
    },
    Ecosystem {
        files: &["mix.lock", "mix.exs", "rebar.config"],
        cache_paths: &[],
        slot_paths: &[],
        job_homes: &[
            ("HEX_HOME", "hex"),
            ("MIX_HOME", "mix"),
            ("REBAR_CACHE_DIR", "rebar3"),
        ],
    },
    Ecosystem {
        files: &["pubspec.lock", "pubspec.yaml"],
        // setup-dart sets PUB_CACHE to this path, so a home would be overwritten
        cache_paths: &["/home/runner/.pub-cache"],
        slot_paths: &[],
        job_homes: &[],
    },
    Ecosystem {
        files: &["*.csproj", "*.fsproj", "*.sln", "global.json"],
        // The whole of ~/.nuget, since NuGet's lock files live in ~/.nuget/lock
        cache_paths: &["/home/runner/.nuget"],
        slot_paths: &[],
        job_homes: &[],
    },
];

impl Act {
    /**
     * Adds the caches of each language whose files the tested tree has, so a job mounts only
     * volumes it can use. A list the owner set in `shipcheck.toml` is kept exactly as written
     **/
    pub fn for_tree<'a>(&mut self, files: impl IntoIterator<Item = &'a str>) {
        let default = Act::default();
        let names: BTreeSet<&str> = files
            .into_iter()
            .map(|f| f.rsplit('/').next().unwrap_or(f))
            .collect();
        let has = |pattern: &str| match pattern.strip_prefix('*') {
            Some(ext) => names.iter().any(|n| n.ends_with(ext)),
            None => names.contains(pattern),
        };
        let (caches, slots, homes) = (
            self.cache_paths == default.cache_paths,
            self.slot_paths == default.slot_paths,
            self.job_homes == default.job_homes,
        );

        for eco in ECOSYSTEMS.iter().filter(|e| e.files.iter().any(|f| has(f))) {
            let add = |list: &mut Vec<String>, paths: &[&str]| {
                for p in paths {
                    if !list.iter().any(|l| l == p) {
                        list.push(p.to_string());
                    }
                }
            };

            if caches {
                add(&mut self.cache_paths, eco.cache_paths);
            }

            if slots {
                add(&mut self.slot_paths, eco.slot_paths);
            }

            if homes {
                for (var, dir) in eco.job_homes {
                    self.job_homes
                        .entry(var.to_string())
                        .or_insert_with(|| Home::Dir(dir.to_string()));
                }
            }
        }
    }

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

/// Memory a job is given in the `max_parallel` cap. flask's jobs peaked at 0.38 GiB, and one job of a Python app
/// at 2.2 GiB still fits beside light ones
pub const JOB_MEMORY: u64 = 1 << 30;

fn default_parallel(host: usize, engine: Option<(u32, u64)>) -> usize {
    let cpus = host.saturating_sub(1);
    let Some((engine_cpus, memory)) = engine else {
        return cpus.max(1);
    };
    let by_memory = usize::try_from(memory / JOB_MEMORY).unwrap_or(usize::MAX);
    cpus.min(engine_cpus as usize).min(by_memory).max(1)
}

impl RepoConfig {
    fn job_runner(&self, job: &Job) -> RunnerKind {
        job.runner.unwrap_or(self.runner)
    }

    /// The `[jobs]` entries that run under `kind`, as `"<file>/<job>"`
    pub fn jobs_on(&self, kind: RunnerKind) -> Vec<String> {
        self.jobs
            .iter()
            .filter(|(_, job)| self.job_runner(job) == kind)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The workflow files act runs jobs of: each of `workflows`, then each file of an act `[jobs]` entry
    pub fn act_files(&self) -> Vec<String> {
        let mut files = self.workflows.clone();

        for name in self.jobs_on(RunnerKind::Act) {
            let file = name.split_once('/').map_or(name.as_str(), |(f, _)| f);

            if !files.iter().any(|f| f == file) {
                files.push(file.to_string());
            }
        }

        files
    }

    /// The engine each act `[jobs]` entry sets, by `"<file>/<job>"`
    pub fn engines(&self) -> BTreeMap<String, Engine> {
        self.jobs
            .iter()
            .filter(|(_, job)| self.job_runner(job) == RunnerKind::Act)
            .filter_map(|(name, job)| Some((name.clone(), job.engine?)))
            .collect()
    }

    /// Some job runs under act
    pub fn acts(&self) -> bool {
        !self.act_files().is_empty()
    }

    /// Some `[host]` check runs natively on this machine
    pub fn hosts(&self) -> bool {
        !self.host.checks.is_empty()
    }

    fn jobs_problems(&self, workflows: &BTreeSet<String>) -> Vec<String> {
        let mut problems = Vec::new();

        if !self.acts() && !self.hosts() {
            problems.push("no workflow, job or host check is set, so nothing would run".into());
        }

        for (name, job) in &self.jobs {
            let kind = self.job_runner(job);
            let file = name
                .split_once('/')
                .filter(|(f, id)| workflows.contains(*f) && !id.is_empty() && !id.contains('/'))
                .map(|(f, _)| f);

            match file {
                None => problems.push(format!(
                    "jobs entry {name:?} must be \"<a workflow file>/<job id>\""
                )),
                // Only a listed workflow runs a job that a host entry could take out of it
                Some(f) if kind == RunnerKind::Host && !self.workflows.iter().any(|w| w == f) => {
                    problems.push(format!(
                        "jobs entry {name:?} is on the host runner, but workflows does not list {f}, so it changes nothing"
                    ))
                }
                Some(_) => {}
            }

            if kind == RunnerKind::Host && job.engine.is_some() {
                problems.push(format!(
                    "jobs entry {name:?} sets engine, which applies to act jobs only"
                ));
            }
        }

        problems
    }

    fn host_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();

        if !self.workflows.is_empty() {
            problems.push(
                "runner is host, which runs [host] checks, so workflows must be empty".into(),
            );
        }

        if self.host.checks.is_empty() {
            problems.push("runner is host, but [host] has no checks, so nothing would run".into());
        }

        problems.extend(self.check_problems());
        problems
    }

    fn check_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();

        // A name becomes a directory and a check named as GitHub names a job, so its grammar
        for (name, command) in &self.host.checks {
            let ok = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c));

            if !ok {
                problems.push(format!(
                    "host check {name:?} must start with a letter or _ and be letters, digits, _ and -"
                ));
            }

            if command.trim().is_empty() {
                problems.push(format!("host check {name:?} has no command"));
            }
        }

        for name in &self.host.env {
            let ok =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

            if !ok || crate::runner::host::refused_env(name) {
                problems.push(format!(
                    "host env {name:?} is refused: only plain names, and none that carries a credential"
                ));
            }
        }

        if self.host.versions.iter().any(|v| v.trim().is_empty()) {
            problems.push("host versions has an empty command".into());
        }

        problems
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|e| format!("shipcheck.toml: {e}"))?;

        // Defaults hide whether these were set, so the raw table says
        if !config.acts() {
            let table: toml::Table =
                toml::from_str(text).map_err(|e| format!("shipcheck.toml: {e}"))?;
            let set: Vec<&str> = ["shard_tests", "network", "act"]
                .into_iter()
                .filter(|k| table.contains_key(*k))
                .collect();

            if !set.is_empty() {
                return Err(format!(
                    "shipcheck.toml: {} only apply to runner = \"act\". Host checks run natively with this machine's network, remove them",
                    set.join(", ")
                ));
            }
        }

        Ok(config)
    }

    /// `max_parallel`, or one job per CPU the engine and the host share, one host CPU left free,
    /// at most one per [`JOB_MEMORY`] of the engine's memory. `engine` is from `docker info`
    pub fn max_parallel(&self, engine: Option<(u32, u64)>) -> usize {
        let host = std::thread::available_parallelism().map_or(1, |n| n.get());
        self.max_parallel
            .unwrap_or_else(|| default_parallel(host, engine))
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

        match self.runner {
            // A config with `[jobs]` mixes runners on purpose
            _ if !self.jobs.is_empty() => {
                problems.extend(self.jobs_problems(workflows));
                problems.extend(self.check_problems());
            }
            RunnerKind::Act if self.workflows.is_empty() => {
                problems.push("workflows is empty, so nothing would run".to_string());
            }
            RunnerKind::Act
                if !self.host.checks.is_empty()
                    || !self.host.versions.is_empty()
                    || !self.host.env.is_empty() =>
            {
                problems.push(
                    "[host] is set, but runner is act. Set runner = \"host\" or remove it".into(),
                );
            }
            RunnerKind::Host => problems.extend(self.host_problems()),
            RunnerKind::Act => {}
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

        if let ShardTests::Fixed(n) = self.shard_tests
            && !(1..=32).contains(&n)
        {
            problems.push(format!(
                "shard_tests is {n}, it must be true, false or 1 to 32"
            ));
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
                .is_some_and(|(f, job)| self.act_files().iter().any(|a| a == f) && !job.is_empty());

            if !ok {
                problems.push(format!(
                    "act.jobs entry {name:?} must be \"<a file in workflows>/<job id>\""
                ));
            }
        }

        // Each path becomes part of a space-separated docker option, so nothing in it may split it
        for path in self.act.cache_paths.iter().chain(&self.act.slot_paths) {
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

    #[test]
    fn a_host_check_is_a_command_or_a_table_that_may_leave_the_sandbox() {
        let cfg = RepoConfig::parse(
            "runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n[host.checks.ios]\nrun = \"xcodebuild test\"\nsandbox = false\n[host.checks.unit]\nrun = \"make test\"\n",
        )
        .unwrap();
        let c = &cfg.host.checks;
        assert_eq!(c["lint"], "make lint");
        assert!(c["lint"].sandbox && c["unit"].sandbox);
        assert_eq!(c["ios"], "xcodebuild test");
        assert!(!c["ios"].sandbox);
        assert!(
            RepoConfig::parse(
                "runner = \"host\"\n[host.checks.ios]\nrun = \"x\"\nsandboxed = false\n"
            )
            .is_err(),
            "a misspelled key is an error, never a sandboxed check"
        );
    }

    #[test]
    fn parallel_jobs_leave_one_host_cpu_and_fit_the_engine() {
        let ample = 64 << 30;
        assert_eq!(default_parallel(14, Some((14, ample))), 13);
        assert_eq!(default_parallel(14, Some((8, ample))), 8);
        assert_eq!(default_parallel(4, Some((4, ample))), 3);
        assert_eq!(default_parallel(1, Some((1, ample))), 1);
        assert_eq!(default_parallel(14, None), 13);
        // Docker Desktop at 16 GB reports 15.6 GiB, room for 15 jobs, so the CPUs decide
        assert_eq!(default_parallel(14, Some((14, 16_745_295_872))), 13);
        // Its default 8 GB reports 7.75 GiB
        assert_eq!(default_parallel(14, Some((14, 8_321_499_136))), 7);
        assert_eq!(default_parallel(14, Some((14, 1 << 30))), 1);
        let fixed = RepoConfig::parse("workflows = [\"ci.yml\"]\nmax_parallel = 3").unwrap();
        assert_eq!(fixed.max_parallel(Some((14, ample))), 3);
    }

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
        assert!(cfg.max_parallel(None) >= 1);
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
    fn parses_host_mode() {
        let cfg = RepoConfig::parse(
            r#"
            runner = "host"
            [host]
            checks = { test = "yarn test", lint = "yarn lint" }
            versions = ["node --version"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.runner, RunnerKind::Host);
        assert!(cfg.workflows.is_empty());
        assert_eq!(cfg.host.checks["test"], "yarn test");
        assert_eq!(cfg.host.versions, ["node --version"]);
        assert_eq!(cfg.validate(&workflows(&["ci.yml"])), Ok(()));
        assert!(
            RepoConfig::parse("runner = \"host\"\n[host]\nchecks = {}\nshell = \"bash\"").is_err()
        );
    }

    #[test]
    fn checks_host_mode() {
        let wf = workflows(&["ci.yml"]);
        let err = |text: &str| RepoConfig::parse(text).unwrap().validate(&wf).unwrap_err();
        assert!(err("runner = \"host\"").contains("no checks"));
        assert!(
            err("runner = \"host\"\nworkflows = [\"ci.yml\"]\n[host]\nchecks = { t = \"x\" }")
                .contains("workflows must be empty")
        );
        let bad = err(
            "runner = \"host\"\n[host]\nchecks = { \"a/b\" = \"x\", 1x = \"x\", ok = \" \" }\nversions = [\"\"]",
        );
        assert!(bad.contains("\"a/b\" must start with a letter"));
        assert!(bad.contains("\"1x\" must start with a letter"));
        assert!(bad.contains("\"ok\" has no command"));
        assert!(bad.contains("empty command"));
        assert!(
            err("workflows = [\"ci.yml\"]\n[host]\nchecks = { t = \"x\" }")
                .contains("runner is act")
        );
        assert!(err("").contains("workflows is empty"));

        for act_only in [
            "shard_tests = true",
            "network = \"none\"",
            "[act]\ncache_paths = []",
        ] {
            let text = format!("runner = \"host\"\n{act_only}\n[host]\nchecks = {{ t = \"x\" }}");
            assert!(
                RepoConfig::parse(&text)
                    .unwrap_err()
                    .contains("only apply to runner = \"act\""),
                "{act_only}"
            );
        }
        let env = err(
            "runner = \"host\"\n[host]\nchecks = { t = \"x\" }\nenv = [\"NODE_OPTIONS\", \"SSH_AUTH_SOCK\", \"NPM_TOKEN\", \"DB_PASSWORD\", \"AWS_PROFILE\", \"A B\"]",
        );
        assert!(!env.contains("NODE_OPTIONS"));

        for refused in [
            "SSH_AUTH_SOCK",
            "NPM_TOKEN",
            "DB_PASSWORD",
            "AWS_PROFILE",
            "A B",
        ] {
            assert!(
                env.contains(&format!("{refused:?} is refused")),
                "{refused}"
            );
        }
    }

    #[test]
    fn parses_a_runner_per_job() {
        let wf = workflows(&["ci.yml", "lint.yml"]);
        let cfg = RepoConfig::parse(
            r#"
            runner = "host"
            network = "internet"
            [host.checks]
            test = "cargo test"
            [jobs."ci.yml/e2e"]
            runner = "act"
            engine = "docker"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.validate(&wf), Ok(()));
        assert!(cfg.acts() && cfg.hosts());
        assert_eq!(cfg.act_files(), ["ci.yml"]);
        assert_eq!(cfg.jobs_on(RunnerKind::Act), ["ci.yml/e2e"]);
        assert_eq!(cfg.jobs["ci.yml/e2e"].engine, Some(Engine::Docker));
        let vm =
            RepoConfig::parse("[jobs.\"ci.yml/e2e\"]\nengine = \"colima\"\n[jobs.\"ci.yml/b\"]")
                .unwrap();
        assert_eq!(
            vm.engines(),
            BTreeMap::from([("ci.yml/e2e".to_string(), Engine::Colima)])
        );

        // Without [jobs], one runner, as before
        let host = RepoConfig::parse("runner = \"host\"\n[host.checks]\nt = \"x\"").unwrap();
        assert!(!host.acts() && host.hosts());
        let act = RepoConfig::parse("workflows = [\"ci.yml\"]").unwrap();
        assert!(act.acts() && !act.hosts());
        assert_eq!(act.act_files(), ["ci.yml"]);

        assert!(RepoConfig::parse("[jobs.\"ci.yml/e2e\"]\nengine = \"podman\"").is_err());
        assert!(RepoConfig::parse("[jobs.\"ci.yml/e2e\"]\nimage = \"x\"").is_err());
        let err = |text: &str| RepoConfig::parse(text).unwrap().validate(&wf).unwrap_err();
        let host = "runner = \"host\"\n[host.checks]\nt = \"x\"\n";

        for bad in ["gone.yml/e2e", "ci.yml", "ci.yml/", "ci.yml/a/b"] {
            let text = format!("{host}[jobs.\"{bad}\"]\nrunner = \"act\"");
            assert!(err(&text).contains(&format!("{bad:?} must be")), "{bad}");
        }

        assert!(
            err(&format!("{host}[jobs.\"ci.yml/e2e\"]\nengine = \"docker\""))
                .contains("applies to act jobs only")
        );
        assert!(
            err("workflows = [\"lint.yml\"]\n[jobs.\"ci.yml/test\"]\nrunner = \"host\"")
                .contains("does not list ci.yml")
        );
        assert!(err("[jobs.\"ci.yml/test\"]\nrunner = \"host\"").contains("nothing would run"));
        // A host entry takes a job out of a listed workflow, the host check of its name stands in for it
        let out = "workflows = [\"ci.yml\"]\n[host.checks]\ntest = \"cargo test\"\n[jobs.\"ci.yml/test\"]\nrunner = \"host\"";
        let cfg = RepoConfig::parse(out).unwrap();
        assert_eq!(cfg.validate(&wf), Ok(()));
        assert_eq!(cfg.jobs_on(RunnerKind::Host), ["ci.yml/test"]);
        // act's settings stay host-only errors only when no job runs under act
        assert!(RepoConfig::parse(&format!("network = \"internet\"\n{host}")).is_err());
    }

    #[test]
    fn parses_logs() {
        let default = RepoConfig::parse(r#"workflows = ["ci.yml"]"#).unwrap();
        assert_eq!(default.logs, Logs::All);
        // A failed job always shows its log, so there is no setting that hides it
        let none = RepoConfig::parse("workflows = [\"ci.yml\"]\nlogs = \"none\"").unwrap_err();
        assert!(none.contains("unknown variant"), "{none}");
        let failed = RepoConfig::parse("workflows = [\"ci.yml\"]\nlogs = \"failed\"").unwrap();
        assert_eq!(failed.logs, Logs::Failed);
        assert!(RepoConfig::parse("workflows = [\"ci.yml\"]\nlogs = \"some\"").is_err());
    }

    #[test]
    fn parses_shard_tests() {
        let parse = |extra: &str| RepoConfig::parse(&format!("workflows = [\"ci.yml\"]\n{extra}"));
        assert!(!parse("").unwrap().shard_tests.enabled());
        assert!(!parse("shard_tests = false").unwrap().shard_tests.enabled());
        assert_eq!(
            parse("shard_tests = true").unwrap().shard_tests,
            ShardTests::On(true)
        );
        assert_eq!(
            parse("shard_tests = 4").unwrap().shard_tests,
            ShardTests::Fixed(4)
        );
        assert!(parse("shard_tests = \"yes\"").is_err());
        assert!(parse("shard_tests = -1").is_err());
        let wf = workflows(&["ci.yml"]);
        assert!(parse("shard_tests = 0").unwrap().validate(&wf).is_err());
        assert!(parse("shard_tests = 33").unwrap().validate(&wf).is_err());
        assert!(parse("shard_tests = 32").unwrap().validate(&wf).is_ok());
    }

    #[test]
    fn adds_caches_only_for_the_languages_in_the_tree() {
        let python = {
            let mut act = Act::default();
            act.for_tree(["pyproject.toml", "uv.lock", "src/app.py"]);
            act
        };
        assert_eq!(python.cache_paths, Act::default().cache_paths);
        assert_eq!(python.slot_paths, Act::default().slot_paths);
        assert_eq!(python.job_homes, Act::default().job_homes);

        let mut act = Act::default();
        act.for_tree([
            "web/yarn.lock",
            "pnpm-lock.yaml",
            "go.sum",
            "java/pom.xml",
            "android/build.gradle.kts",
            "build.sbt",
            "Gemfile.lock",
            "composer.lock",
            "mix.lock",
            "app/pubspec.lock",
            "src/App.csproj",
        ]);
        for p in [
            "/home/runner/.npm",
            "/home/runner/go/pkg/mod",
            "/home/runner/.pub-cache",
            "/home/runner/.nuget",
            "/home/runner/.yarn/berry/cache",
        ] {
            assert!(act.cache_paths.iter().any(|c| c == p), "{p}");
        }
        for p in [
            "/var/lib/apt/lists",
            "/home/runner/.m2",
            "/home/runner/.gradle",
            "/home/runner/.sbt",
            "/home/runner/.cache/yarn",
            "/home/runner/.local/share/nub",
        ] {
            assert!(act.slot_paths.iter().any(|c| c == p), "{p}");
            assert!(!act.cache_paths.iter().any(|c| c == p), "{p} is shared");
        }
        for var in [
            "npm_config_store_dir",
            "COMPOSER_CACHE_DIR",
            "BUNDLE_USER_CACHE",
            "HEX_HOME",
            "MIX_HOME",
            "REBAR_CACHE_DIR",
            "CARGO_HOME",
            "RUNNER_TOOL_CACHE",
            "AGENT_TOOLSDIRECTORY",
        ] {
            assert!(act.job_homes.contains_key(var), "{var}");
        }
        assert_eq!(act.slot_paths.len(), 6, "no path twice");

        let mut npm = Act::default();
        npm.for_tree(["package-lock.json", "a.gemspec"]);
        assert_eq!(npm.slot_paths[1], "/home/runner/.local/share/nub");
        assert!(npm.job_homes.contains_key("BUNDLE_USER_CACHE"));
        assert!(!npm.job_homes.contains_key("npm_config_store_dir"));

        // A list the owner set is kept as written
        let mut own = RepoConfig::parse("workflows = [\"ci.yml\"]\n[act]\nslot_paths = []")
            .unwrap()
            .act;
        own.for_tree(["pom.xml", "go.sum"]);
        assert!(own.slot_paths.is_empty());
        assert!(
            own.cache_paths
                .iter()
                .any(|c| c == "/home/runner/go/pkg/mod")
        );

        let cfg = RepoConfig::parse("workflows = [\"ci.yml\"]").unwrap();
        assert_eq!(cfg.validate(&workflows(&["ci.yml"])), Ok(()));
        let bad =
            RepoConfig::parse("workflows = [\"ci.yml\"]\n[act]\nslot_paths = [\"/a b\"]").unwrap();
        assert!(
            bad.validate(&workflows(&["ci.yml"]))
                .unwrap_err()
                .contains("\"/a b\" must be absolute")
        );
    }

    #[test]
    fn team_is_off_by_default_and_refuses_unknown_keys() {
        let off = RepoConfig::parse("workflows = [\"ci.yml\"]").unwrap();
        assert_eq!(off.team, Team::default());
        assert!(!off.team.records && !off.team.cache && !off.team.logs);
        let on = RepoConfig::parse("workflows = [\"ci.yml\"]\n[team]\nrecords = true").unwrap();
        assert!(on.team.records && !on.team.cache);
        assert!(RepoConfig::parse("workflows = [\"ci.yml\"]\n[team]\nrecord = true").is_err());
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
            cache_paths = ["/home/runner/.cargo", "/x --privileged", "relative"]
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
        assert!(!err.contains("/home/runner/.cargo"));
        assert!(!err.contains("GOMODCACHE"));

        for bad in ["\"A;B\"", "\"X\"", "\"Y\""] {
            assert!(err.contains(&format!("job_homes entry {bad}")), "{bad}");
        }
    }
}
