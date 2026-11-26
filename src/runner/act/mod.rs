use crate::{cache, config, paths};
use matrix::expand_matrices;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};
use yaml_rust2::Yaml;
use yaml_rust2::yaml::Hash;

use super::{Outcome, RunRequest, Runner, gc, shard};
use crate::config::JOB_HOMES;
use crate::workflow::{self, key, step};

mod matrix;

mod docker;
pub use docker::{Docker, here, here_parts, home_prefix, prefix_of, run_tags};

/// Runs workflows with nektos/act, each job in a Docker container
#[derive(Default, Debug)]
pub struct Act {
    config: config::Act,
    network: config::Network,
    shards: config::ShardTests,
    /// The engine of each `[jobs]` entry that sets one, by `"<file>/<job>"`
    engines: BTreeMap<String, config::Engine>,
}

impl Act {
    pub fn new(config: config::Act) -> Self {
        Act {
            config,
            network: config::Network::None,
            shards: config::ShardTests::default(),
            engines: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_engines(self, engines: BTreeMap<String, config::Engine>) -> Self {
        Act { engines, ..self }
    }

    /// What an engine reads of `[act]` to run a job of `file`
    pub fn settings(&self, file: &str) -> JobSettings<'_> {
        // The labels this host has an image for, the default ones and each `[act.jobs]` job's own
        let labels = platforms(&self.config)
            .into_keys()
            .chain(
                self.config
                    .job_images(file)
                    .into_keys()
                    .map(|j| job_label(&j)),
            )
            .collect();
        JobSettings {
            config: &self.config,
            network: self.network,
            shards: self.shards,
            labels,
        }
    }

    fn uses_colima(&self) -> bool {
        self.engines.values().any(|e| *e == config::Engine::Colima)
    }

    #[must_use]
    pub fn with_network(self, network: config::Network) -> Self {
        Act { network, ..self }
    }

    #[must_use]
    pub fn with_shards(self, shards: config::ShardTests) -> Self {
        Act { shards, ..self }
    }
}

/// The label a job gets when `[act.jobs]` gives it an image of its own
pub fn job_label(job: &str) -> String {
    format!("shipcheck-{job}")
}

/// What an engine reads of `[act]` to run one job, so no engine needs the runner itself
#[derive(Debug)]
pub struct JobSettings<'a> {
    config: &'a config::Act,
    network: config::Network,
    shards: config::ShardTests,
    labels: Vec<String>,
}

impl JobSettings<'_> {
    pub fn config(&self) -> &config::Act {
        self.config
    }

    pub fn network(&self) -> config::Network {
        self.network
    }

    pub fn shards(&self) -> config::ShardTests {
        self.shards
    }

    pub fn labels(&self) -> &[String] {
        &self.labels
    }
}

/**
 * What an act job's steps run on. Preparing the workflow, the markers and reading the results are
 * act's and the same on every engine, an engine starts act for one job and cleans up after it
 **/
pub trait Engine: Sync {
    fn name(&self) -> &'static str;

    /// What the cache key holds of the engine. Docker's is none, so keys from before it stay
    fn identity(&self) -> Result<String, String> {
        Ok(String::new())
    }

    /// Refuses a host whose isolation is known to be broken
    fn preflight(&self) -> Result<(), String>;

    /// Runs one job of the prepared workflow with act, as [`Runner::run`]
    fn run(
        &self,
        settings: &JobSettings<'_>,
        request: &RunRequest<'_>,
        log: &Path,
    ) -> Result<Outcome, String>;

    /// Removes what a dead run left for this job's checkout
    fn reclaim(&self, run_dir: &Path);

    /// Removes this engine's state that no run used for a while
    fn prune(&self) -> Vec<String>;

    /// Removes this engine's caches for one repository, or for all of them
    fn clean(&self, repo_id: Option<&str>) -> Vec<String>;
}

impl Act {
    /// The engine of a job, or of the matrix job a leg `<job>__<n>` came from
    fn engine(&self, file: &str, job: &str) -> &dyn Engine {
        let base = job.split("__").next().unwrap_or(job);

        match self.engines.get(&format!("{file}/{base}")) {
            Some(config::Engine::Colima) => &colima::Colima,
            _ => &Docker,
        }
    }
}

impl Runner for Act {
    fn preflight(&self) -> Result<(), String> {
        // A job that a colima job needs runs on Docker, so Docker is checked either way
        Docker.preflight()?;

        if self.uses_colima() {
            colima::Colima.preflight()?;
        }

        // act before 0.2.86 served its cache on every interface and honoured ::set-env (CVE-2026-34041, -34042)
        let act = self.identity()?;

        match act.rsplit(' ').next().and_then(version) {
            Some(v) if v < (0, 2, 86) => Err(format!(
                "{act} has known holes (CVE-2026-34041, CVE-2026-34042). Update act to 0.2.86 or later"
            )),
            _ => Ok(()),
        }
    }

    fn identity(&self) -> Result<String, String> {
        // From `/`, so the .actrc of whatever repo the caller stands in cannot break it
        let out = Command::new("act")
            .arg("--version")
            .current_dir("/")
            .output()
            .map_err(|e| format!("act: {e}"))?;
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();

        if !out.status.success() || version.is_empty() {
            return Err(format!(
                "act --version failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        if self.uses_colima() {
            let colima = &colima::Colima;
            return Ok(format!(
                "{version}\0{}\0{}",
                colima.name(),
                colima.identity()?
            ));
        }

        Ok(version)
    }

    fn prepare(&self, source: &str, file: &str) -> Result<String, String> {
        self.prepare_workflow(source, file)
    }

    fn run(&self, request: &RunRequest<'_>, log_path: &Path) -> Result<Outcome, String> {
        self.engine(request.file, request.job)
            .run(&self.settings(request.file), request, log_path)
    }

    // colima's are checked on disk first, so a machine without its VMs pays nothing
    fn reclaim(&self, run_dir: &Path) {
        Docker.reclaim(run_dir);
        colima::Colima.reclaim(run_dir);
    }

    fn prune(&self) -> Vec<String> {
        let mut done = Docker.prune();
        done.extend(colima::Colima.prune());
        done
    }

    fn clean(&self, repo_id: Option<&str>) -> Vec<String> {
        let mut done = Docker.clean(repo_id);
        done.extend(colima::Colima.clean(repo_id));
        done
    }
}

impl Act {
    /// Adds the `workflow_dispatch` trigger act runs by, expands matrices, gives jobs named in
    /// `[act.jobs]` their own `runs-on` label, and gives each job its own tool homes
    fn prepare_workflow(&self, source: &str, file: &str) -> Result<String, String> {
        let mut root = workflow::load(source)?;

        let dispatch = key("workflow_dispatch");
        let Some(on) = root.get_mut(&key("on")) else {
            return Err("workflow has no `on` triggers".into());
        };
        *on = match std::mem::replace(on, Yaml::Null) {
            Yaml::String(event) => Yaml::Hash(Hash::from_iter([
                (key(&event), Yaml::Null),
                (dispatch, Yaml::Null),
            ])),
            Yaml::Array(mut events) => {
                if !events.contains(&dispatch) {
                    events.push(dispatch);
                }

                Yaml::Array(events)
            }
            Yaml::Hash(mut events) => {
                events.entry(dispatch).or_insert(Yaml::Null);
                Yaml::Hash(events)
            }
            other => {
                return Err(format!("workflow `on` is not a trigger list: {other:?}"));
            }
        };

        let root_shell = shard::default_shell(&root);
        let top_env = root.get(&key("env")).cloned().unwrap_or(Yaml::Null);
        let jobs = workflow::jobs_mut(&mut root)?;
        *jobs = expand_matrices(std::mem::take(jobs));

        if self.shards.enabled() {
            shard::shard_steps(jobs, root_shell.as_deref(), self.shards);
        }

        // After expansion, so `container: ${{ matrix.c }}` is checked as the value it becomes
        refuse_escapes(jobs, self.network)?;
        let own_image = self.config.job_images(file);

        for (id, job) in jobs.iter_mut() {
            let base = id.as_str().map(|s| s.split("__").next().unwrap_or(s));

            if let Yaml::Hash(job) = job
                && let Some(rest) = job
                    .get(&key("if"))
                    .and_then(Yaml::as_str)
                    .and_then(workflow::ungate)
            {
                if rest.is_empty() {
                    job.remove(&key("if"));
                } else {
                    job.insert(key("if"), key(&rest));
                }
            }

            if let (Some(base), Yaml::Hash(job)) = (base, job)
                && own_image.contains_key(base)
            {
                job.insert(key("runs-on"), key(&job_label(base)));
            }
        }

        let homes = &self.config.job_homes;
        let path_lines: String = homes
            .iter()
            .filter_map(|(var, home)| {
                let dir = format!("${var}/{}", home.path()?);
                Some(format!(
                    "mkdir -p \"{dir}\"\necho \"{dir}\" >> \"$GITHUB_PATH\"\n"
                ))
            })
            .collect();
        // apt runs under sudo, so its lists stay root's
        let mounts: String = (self.config.cache_paths.iter())
            .chain(&self.config.slot_paths)
            .filter(|p| *p != "/var/lib/apt/lists")
            .fold(String::new(), |mut s, p| {
                let _ = write!(s, "\"{p}\" ");
                s
            });
        // Docker creates a new volume and the missing dirs above it owned by root, and the job runs
        // as `runner`, which has sudo. setup-go then writes `~/go/bin` beside `~/go/pkg/mod`
        let own_mounts = format!(
            "for d in {mounts}\"$GITHUB_WORKSPACE/.venv\"; do\n  \
             while [ -w \"$d\" ] || sudo -n chown \"$(id -u):$(id -g)\" \"$d\" || true; do\n    \
             d=$(dirname \"$d\")\n    \
             case $d in \"${{HOME:-/none}}\"/*) ;; *) break ;; esac\n  \
             done\ndone\n\
             mkdir -p \"$RUNNER_TEMP\" || true\n"
        );
        let label_latest = !self.config.platforms.contains_key("ubuntu-latest");

        for (id, job) in jobs.iter_mut() {
            let (Some(id), Yaml::Hash(job)) = (id.as_str(), job) else {
                continue;
            };

            if !matches!(job.get(&key("steps")), Some(Yaml::Array(_))) {
                continue;
            }

            let env = job
                .entry(key("env"))
                .or_insert_with(|| Yaml::Hash(Hash::new()));

            if let Yaml::Hash(env) = env
                && !env.contains_key(&key("GIT_CONFIG_COUNT"))
            {
                // The mounted repository is owned by another user than the job's, which git refuses
                for (k, v) in [
                    ("GIT_CONFIG_COUNT", "1"),
                    ("GIT_CONFIG_KEY_0", "safe.directory"),
                    ("GIT_CONFIG_VALUE_0", "*"),
                ] {
                    env.insert(key(k), key(v));
                }
            }

            // Each job gets its own tool homes in the shared cache volume, so parallel jobs never
            // install into one toolchain or race on one lock
            if let Yaml::Hash(env) = env {
                for (var, home) in homes {
                    env.entry(key(var))
                        .or_insert_with(|| key(&format!("{JOB_HOMES}/{id}/{}", home.dir())));
                }
            }

            // GitHub's runner keeps it in the home, act gives /tmp, a container job keeps act's
            if !job.contains_key(&key("container"))
                && let Some(Yaml::Hash(env)) = job.get_mut(&key("env"))
            {
                env.entry(key("RUNNER_TEMP"))
                    .or_insert_with(|| key("/home/runner/work/_temp"));
            }

            // act sets ImageOS from the label and takes `ubuntu-latest` for ubuntu20
            if label_latest && job.get(&key("runs-on")) == Some(&key("ubuntu-latest")) {
                job.insert(key("runs-on"), key("ubuntu-24.04"));
            }

            let job_env = job.get(&key("env")).cloned().unwrap_or(Yaml::Null);

            if let Some(Yaml::Array(steps)) = job.get_mut(&key("steps")) {
                *steps = std::mem::take(steps)
                    .into_iter()
                    .flat_map(|s| guarded(s, [&job_env, &top_env]))
                    .collect();
                steps.insert(
                    0,
                    step("shipcheck homes", None, format!("{own_mounts}{path_lines}")),
                );
            }
        }

        for job in jobs.values_mut() {
            if let Some(step) = job.as_hash().and_then(outputs_step)
                && let Some(Yaml::Array(steps)) =
                    job.as_mut_hash().and_then(|j| j.get_mut(&key("steps")))
            {
                steps.push(step);
            }
        }

        workflow::emit(root)
    }
}

/**
 * The `if:` of a guard before a step whose `if:` reads what act's `workflow_dispatch` and a PR
 * answer differently: the event, the ref, the actor or `vars`. The guard runs, and fails the job,
 * when act skips the step on a job that still passes. `always()` and `!cancelled()` count as true.
 * None when the step is skipped on every PR, or runs only on a failure, as GitHub answers the same
 **/
fn step_guard(cond: &str, env: impl Fn(&str) -> Option<String>) -> Option<String> {
    let c = cond.trim();
    let c = c
        .strip_prefix("${{")
        .and_then(|c| c.strip_suffix("}}"))
        .unwrap_or(c)
        .trim();
    let reads_pr = |text: &str| {
        [
            "github.event",
            "github.ref",
            "github.head_ref",
            "github.base_ref",
            "github.actor",
            "github.triggering_actor",
            "vars.",
        ]
        .iter()
        .any(|p| text.contains(p))
    };
    // An `env.NAME` reads what its value reads, as the step's, the job's or the workflow's `env` sets it
    let names = c.match_indices("env.").filter_map(|(at, _)| {
        let before = c[..at].chars().next_back();

        if before.is_some_and(|b| b.is_alphanumeric() || b == '_' || b == '.') {
            return None;
        }

        let name: String = c[at + 4..]
            .chars()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '-')
            .collect();
        (!name.is_empty()).then_some(name)
    });
    let reads = reads_pr(c)
        || names
            .into_iter()
            .any(|n| env(&n).is_some_and(|v| reads_pr(&v)));
    let passing = c
        .replace("!cancelled()", "true")
        .replace("always()", "true");

    if !reads
        || workflow::false_on_every_pr(c)
        || passing.contains("failure()")
        || passing.contains("cancelled()")
    {
        return None;
    }

    Some(format!("${{{{ success() && !({passing}) }}}}"))
}

/// A step, after the guard `step_guard` gives it. The guard has no `id`, so no step reads it
fn guarded(s: Yaml, outer_env: [&Yaml; 2]) -> Vec<Yaml> {
    // Every level's value counts, since a step's own `env` may not reach its `if:`
    let env = |name: &str| {
        let values: Vec<&str> = [&s["env"], outer_env[0], outer_env[1]]
            .into_iter()
            .filter_map(|e| e[name].as_str())
            .collect();
        (!values.is_empty()).then(|| values.join("\n"))
    };
    let Some(cond) = s["if"].as_str().and_then(|c| step_guard(c, env)) else {
        return vec![s];
    };
    let name = (s["name"].as_str())
        .or(s["uses"].as_str())
        .or(s["run"].as_str().and_then(|r| r.lines().next()))
        .unwrap_or("a step")
        .to_string();
    let mut guard = step(
        "shipcheck if",
        Some(&cond),
        "echo \"::error::act skipped the step '$STEP' by its if:, which may hold on a PR, and only GitHub can tell\"\nexit 1".into(),
    );

    if let Yaml::Hash(h) = &mut guard {
        h.insert(
            key("env"),
            Yaml::Hash(Hash::from_iter([(key("STEP"), key(&name))])),
        );
    }

    vec![guard, s]
}

/// The image for each label: the defaults below, then `[act] platforms` over them
pub fn platforms(config: &config::Act) -> BTreeMap<String, String> {
    DEFAULT_PLATFORMS
        .into_iter()
        .map(|(l, i)| (l.to_string(), i.to_string()))
        .chain(config.platforms.clone())
        .collect()
}

/// Whether act, run again as `cmd` with `--dryrun --verbose`, says the job's `if:` skips it
pub(super) fn skipped_by_if(cmd: &Command) -> bool {
    let mut dry = Command::new(cmd.get_program());
    dry.args(cmd.get_args())
        .args(["--dryrun", "--verbose"])
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null());

    for (k, v) in cmd.get_envs() {
        if let Some(v) = v {
            dry.env(k, v);
        }
    }

    if let Some(dir) = cmd.get_current_dir() {
        dry.current_dir(dir);
    }

    dry.output().is_ok_and(|out| {
        out.status.success() && dry_run_skips(&String::from_utf8_lossy(&out.stdout))
    })
}

/// act's verbose line for a job whose `if:` is false
pub(super) fn dry_run_skips(log: &str) -> bool {
    log.lines()
        .any(|l| l.contains("] [DEBUG] Skipping job '") && l.contains("' due to '"))
}

/// catthehacker's `runner-*` images, pinned by index digest for amd64 and arm64, unless
/// `[act] platforms` maps the label. They run as `runner`, uid 1001, with sudo, as GitHub does.
/// Without them act stops to ask for a default image, and 0.2.89 has none for `ubuntu-24.04`.
/// `ubuntu-slim` is 24.04 with fewer tools, and an `-arm` label runs the index's image for this host
const DEFAULT_PLATFORMS: [(&str, &str); 6] = [
    ("ubuntu-latest", UBUNTU_24),
    ("ubuntu-24.04", UBUNTU_24),
    ("ubuntu-slim", UBUNTU_24),
    ("ubuntu-24.04-arm", UBUNTU_24),
    ("ubuntu-22.04", UBUNTU_22),
    ("ubuntu-22.04-arm", UBUNTU_22),
];
const UBUNTU_24: &str =
    "catthehacker/ubuntu@sha256:27043bae70b35fd8a5af1ca3a7e3e45e33595a9b7701cd7cc9232027d9fca773";
const UBUNTU_22: &str =
    "catthehacker/ubuntu@sha256:942b477bcb64e8b32c2f9bc86c904a016d84d07d48b3a89554d1c317b971302f";

/// busybox 1.37, pinned, for handing a checkout between the user and the job on Linux
const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// The uid and gid of `runner` in the default images and on GitHub
const RUNNER_ID: &str = "1001:1001";

/**
 * On Linux a bind mount keeps the host's uids, so the job's `runner` cannot write a checkout the
 * user owns, and the user cannot empty, warm or clean the files the job wrote. A throwaway
 * container hands the tree over before the job and back after it. Docker Desktop on a Mac maps
 * ownership already
 **/
/// The CPUs and bytes of memory the Docker engine has, which on a Mac are the VM's, not the host's
pub fn engine_resources() -> Option<(u32, u64)> {
    let out = Command::new("docker")
        .args(["info", "--format", "{{.NCPU}} {{.MemTotal}}"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (cpus, memory) = text.trim().split_once(' ')?;
    Some((cpus.parse().ok()?, memory.parse().ok()?))
}

/// Where act keeps the actions it fetched, per job below this, for one repository or every one
fn actions_dir(repo_id: Option<&str>) -> PathBuf {
    let dir = paths::home().join("actions");
    repo_id.map_or(dir.clone(), |id| dir.join(id))
}

/// The job's own action cache, since parallel act processes cloning one action into one cache
/// corrupt it. Canonical, since act 0.2.89 runs a post step by the cleaned path, so a `..` failed it
pub(super) fn job_actions(request: &RunRequest<'_>) -> Result<PathBuf, String> {
    let actions = actions_dir(Some(request.repo_id))
        .join(request.file)
        .join(request.job);
    fs::create_dir_all(&actions).map_err(|e| format!("{}: {e}", actions.display()))?;
    actions
        .canonicalize()
        .map_err(|e| format!("{}: {e}", actions.display()))
}

/// Whether every action act cloned into `dir`, one `<uses>@<ref>` folder each, still resolves its ref
pub(super) fn cached_actions_resolve(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return true;
    };

    entries.flatten().all(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((_, git_ref)) = name.rsplit_once('@') else {
            return true;
        };

        Command::new("git")
            .arg("--git-dir")
            .arg(entry.path().join(".git"))
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{git_ref}^{{commit}}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Where older versions kept act's `actions/cache` entries, which `shipcheck clean` still removes
fn cache_dir(repo_id: Option<&str>) -> PathBuf {
    let dir = paths::home().join("actcache");
    repo_id.map_or(dir.clone(), |id| dir.join(id))
}

fn version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim_start_matches('v').split(['.', '-', ' ']);
    let mut next = || parts.next()?.parse().ok();
    Some((next()?, next()?, next().unwrap_or(0)))
}

/**
 * A known way out of the container on this engine, from `docker version` printed as
 * `<platform>|<component>=<version>;...`. Docker Desktop before 4.44.3 served its Engine API to
 * every container (CVE-2025-9074), runc before 1.2.8 and 1.3.3 had escapes (Leaky Vessels and the
 * November 2025 CVEs). A version that cannot be read is not a known problem
 **/
fn engine_problem(report: &str) -> Option<String> {
    let (platform, components) = report.split_once('|').unwrap_or((report, ""));

    if let Some(desktop) = platform.strip_prefix("Docker Desktop ").and_then(version)
        && desktop < (4, 44, 3)
    {
        return Some(format!(
            "{platform} lets any container reach the Docker engine (CVE-2025-9074). Update Docker Desktop to 4.44.3 or later"
        ));
    }

    let runc = components
        .split(';')
        .find_map(|c| c.strip_prefix("runc="))?;
    let (major, minor, patch) = version(runc)?;
    let rc = runc
        .split("-rc.")
        .nth(1)
        .and_then(|n| n.parse::<u32>().ok());
    let escapes = (major, minor) < (1, 2)
        || (major, minor) == (1, 2) && patch < 8
        || (major, minor) == (1, 3) && patch < 3
        || (major, minor, patch) == (1, 4, 0) && rc.is_some_and(|n| n < 3);

    escapes.then(|| {
        format!("runc {runc} has known container escapes. Update the Docker engine to one with runc 1.2.8, 1.3.3 or later")
    })
}

/// The first `${{ }}` anywhere in a value, which act may expand into options shipcheck never saw
fn expression_in(value: &Yaml, path: &str) -> Option<String> {
    match value {
        Yaml::String(s) if s.contains("${{") => Some(path.to_string()),
        Yaml::Hash(h) => h.iter().find_map(|(k, v)| {
            expression_in(v, &format!("{path}.{}", k.as_str().unwrap_or_default()))
        }),
        Yaml::Array(items) => items.iter().find_map(|v| expression_in(v, path)),
        _ => None,
    }
}

/// The first `${{ }}` in one container's or service's settings, where only its own `image` and
/// `env` may hold one, since those never become Docker options
fn settings_expression(settings: &Yaml, path: &str) -> Option<String> {
    let Yaml::Hash(h) = settings else {
        return expression_in(settings, path);
    };

    h.iter().find_map(|(k, v)| {
        let k = k.as_str().unwrap_or_default();
        (k != "image" && k != "env")
            .then(|| expression_in(v, &format!("{path}.{k}")))
            .flatten()
    })
}

/// Options may hold only these characters, so no escape or quoting trick reads differently to
/// act's shell-style splitter than to shipcheck's
fn plain_options(text: &str) -> bool {
    let quotes_closed =
        text.matches('"').count().is_multiple_of(2) && text.matches('\'').count().is_multiple_of(2);
    quotes_closed
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " _=.,:/@+%-\"'".contains(c))
}

/// The only Docker options a workflow may give a container: limits, environment, health checks
/// and process settings. Anything else could reach the host's files, network, devices or kernel
const ALLOWED_OPTIONS: [&str; 22] = [
    "--cpus",
    "--memory",
    "-m",
    "--memory-swap",
    "--shm-size",
    "--env",
    "-e",
    "--health-cmd",
    "--health-interval",
    "--health-timeout",
    "--health-retries",
    "--health-start-period",
    "--no-healthcheck",
    "--user",
    "-u",
    "--workdir",
    "-w",
    "--entrypoint",
    "--hostname",
    "-h",
    "--ulimit",
    "--init",
];

/// Splits options like a shell does, so a quoted value such as `--health-cmd "pg_isready -U x"`
/// is one word and its `-U` is not taken for a flag
fn shell_words(text: &str) -> Vec<String> {
    let (mut words, mut word, mut quote) = (Vec::new(), String::new(), None);

    for c in text.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(c),
        }
    }

    if !word.is_empty() {
        words.push(word);
    }

    words
}

/**
 * A job's `container.options` and its services' `options` go to Docker as they are, past every
 * flag shipcheck sets, so one `-v /:/host` or `--network host` undoes the isolation. Workflows come
 * from the base, yet shipcheck still refuses them, and services when the job has no network
 **/
fn refuse_escapes(jobs: &Hash, network: config::Network) -> Result<(), String> {
    for (id, job) in jobs {
        let id = id.as_str().unwrap_or_default();

        // act runs a called workflow's jobs, container options included, without this check, and
        // reads a local one from the branch or clones a remote one on the host
        if !job["uses"].is_badvalue() {
            return Err(format!(
                "job {id} calls a reusable workflow, which shipcheck cannot check before it runs. Leave this workflow out of `workflows`"
            ));
        }

        let mut options = vec![("container", &job["container"]["options"])];

        // act evaluates expressions after this check, so `options: ${{ matrix.o }}` would pass it.
        // A matrix reference in `container` is already resolved, any expression left can be a mapping
        let container = match &job["container"] {
            Yaml::String(s) if s.contains("${{") => {
                return Err(format!(
                    "job {id} sets its container with an expression, which shipcheck cannot check before it runs"
                ));
            }
            Yaml::String(_) => &Yaml::Null,
            other => other,
        };
        let services = match &job["services"] {
            Yaml::Hash(s) => s.iter().find_map(|(name, service)| {
                let name = name.as_str().unwrap_or_default();
                settings_expression(service, &format!("services.{name}"))
            }),
            other => expression_in(other, "services"),
        };

        if let Some(path) = settings_expression(container, "container").or(services) {
            return Err(format!(
                "job {id} sets {path} with an expression, which shipcheck cannot check before it runs"
            ));
        }

        if !matches!(job["services"], Yaml::Hash(_) | Yaml::BadValue | Yaml::Null) {
            return Err(format!("job {id} has services that are not a mapping"));
        }

        // `volumes` bind host paths, which no job may have
        let containers = std::iter::once(&job["container"]).chain(
            job["services"]
                .as_hash()
                .into_iter()
                .flat_map(|s| s.values()),
        );

        for container in containers {
            if !matches!(container["volumes"], Yaml::BadValue | Yaml::Null) {
                return Err(format!(
                    "job {id} sets volumes on a container, which would reach the host. shipcheck refuses it"
                ));
            }
        }

        if let Yaml::Hash(services) = &job["services"] {
            if network == config::Network::None {
                return Err(format!(
                    "job {id} has services, which need a network. Set network = \"internet\" in shipcheck.toml"
                ));
            }

            options.extend(services.values().map(|s| ("service", &s["options"])));
        }

        for (what, value) in options {
            let text = match value {
                Yaml::String(text) => text,
                Yaml::BadValue | Yaml::Null => continue,
                _ => {
                    return Err(format!("job {id} has {what} options that are not a string"));
                }
            };

            if !plain_options(text) {
                return Err(format!(
                    "job {id} has {what} options with characters other than letters, digits, spaces, closed quotes and _=.,:/@+%-"
                ));
            }

            for word in shell_words(text).iter().filter(|w| w.starts_with('-')) {
                let flag = word.split('=').next().unwrap_or(word);

                if !ALLOWED_OPTIONS.contains(&flag) {
                    return Err(format!(
                        "job {id} sets {flag} in its {what} options, which could reach the host. shipcheck allows only {}",
                        ALLOWED_OPTIONS.join(" ")
                    ));
                }
            }
        }
    }

    Ok(())
}

fn is_id(s: &str) -> bool {
    !s.is_empty() && ident_len(s) == s.len()
}

fn ident_len(s: &str) -> usize {
    s.bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
        .count()
}

/// A last step that prints the job's `outputs`, hex-encoded so any value survives the log
fn outputs_step(job: &Hash) -> Option<Yaml> {
    let Some(Yaml::Hash(outputs)) = job.get(&key("outputs")) else {
        return None;
    };
    let mut env = Hash::new();
    let mut script = String::new();

    for (i, (name, value)) in outputs.iter().enumerate() {
        let value = match value {
            Yaml::String(s) | Yaml::Real(s) => s.clone(),
            Yaml::Integer(n) => n.to_string(),
            Yaml::Boolean(b) => b.to_string(),
            _ => continue,
        };
        let Some(name) = name.as_str().filter(|n| is_id(n)) else {
            continue;
        };
        let var = format!("SHIPCHECK_OUTPUT_{i}");
        env.insert(key(&var), key(&value));
        let _ = writeln!(
            script,
            "printf 'shipcheck-output %s %s\\n' '{name}' \"$(printf %s \"${var}\" | od -An -v -tx1 | tr -d ' \\n')\""
        );
    }

    if script.is_empty() {
        return None;
    }

    let mut step = step("shipcheck outputs", None, script);

    if let Yaml::Hash(h) = &mut step {
        h.insert(key("env"), Yaml::Hash(env));
    }

    Some(step)
}

/// The outputs a job printed with its outputs step, the last value of each name
pub(super) fn read_outputs(log: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();

    for line in log.lines() {
        let Some((_, rest)) = line.split_once("shipcheck-output ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let Some(name) = words.next().filter(|n| is_id(n)) else {
            continue;
        };
        let hex = words.next().unwrap_or("");

        if words.next().is_some() || hex.len() % 2 != 0 {
            continue;
        }

        let bytes: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect();

        if let Some(bytes) = bytes {
            out.insert(
                name.to_string(),
                String::from_utf8_lossy(&bytes).into_owned(),
            );
        }
    }

    out
}

/**
 * act empties `$GITHUB_STEP_SUMMARY` before each step and logs what the step left in it as
 * `[<workflow>/<job>]   ⚙  Summary - <first line>`, the other lines bare. A step's own output
 * has `|` after the prefix, so it cannot pass for one. The summary ends at the next prefixed line
 **/
pub(super) fn read_summary(log: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut open: Option<&str> = None;

    for line in log.lines() {
        if let Some(prefix) = open {
            if line.starts_with(prefix) {
                open = None;
            } else if let Some(last) = parts.last_mut() {
                last.push('\n');
                last.push_str(line);
                continue;
            }
        }

        let Some(end) = line.find(']').filter(|_| line.starts_with('[')) else {
            continue;
        };

        if let Some(first) = line[end + 1..].strip_prefix("   \u{2699}  Summary - ") {
            open = Some(&line[..=end]);
            parts.push(first.to_string());
        }
    }

    super::cap_summary(&parts.join("\n"))
}

/**
 * A value as an expression that holds none of its quotes, braces or `$`. act finds `${{` and `}}`
 * even inside a quoted string in some places, such as a bare `if`, so a quoted value could close
 * the expression and open one of its own. Every character but letters, digits and spaces is a JSON
 * `\u` escape
 **/
fn literal(value: &str) -> String {
    let mut json = String::new();

    for c in value.chars() {
        if c.is_ascii_alphanumeric() || c == ' ' {
            json.push(c);
        } else {
            let mut units = [0; 2];

            for unit in c.encode_utf16(&mut units) {
                let _ = write!(json, "\\u{unit:04x}");
            }
        }
    }

    format!("fromJSON('\"{json}\"')")
}

/// Replaces `needs.<job>.outputs.<name>` and `needs.<job>.result` with literals
fn replace_needs(s: &str, needs: &BTreeMap<String, BTreeMap<String, String>>) -> String {
    let mut out = String::new();
    let mut rest = s;

    while let Some(i) = rest.find("needs.") {
        let boundary = rest[..i]
            .bytes()
            .last()
            .is_none_or(|b| !(b.is_ascii_alphanumeric() || b"_-.".contains(&b)));
        out.push_str(&rest[..i]);
        let after = &rest[i + "needs.".len()..];
        let (name, tail) = after.split_at(ident_len(after));

        if let (true, Some(outputs)) = (boundary, needs.get(name)) {
            if let Some(k) = tail.strip_prefix(".outputs.")
                && ident_len(k) > 0
            {
                let (k, t) = k.split_at(ident_len(k));
                out.push_str(&literal(outputs.get(k).map_or("", String::as_str)));
                rest = t;
                continue;
            }

            if let Some(t) = tail.strip_prefix(".result")
                && ident_len(t) == 0
            {
                out.push_str("'success'");
                rest = t;
                continue;
            }
        }

        out.push_str("needs.");
        rest = after;
    }

    out.push_str(rest);
    out
}

/// Whether a YAML value is the value of an `if` key, where GitHub reads a bare expression
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    If,
    Other,
}

impl Key {
    fn of(k: &Yaml) -> Key {
        if k.as_str() == Some("if") {
            Key::If
        } else {
            Key::Other
        }
    }
}

/// Replaces needs only inside `${{ }}` and in a bare `if`, the places GitHub reads them. Elsewhere a
/// value would be plain text where act still expands any `${{ }}` the value holds
fn substitute(y: &mut Yaml, needs: &BTreeMap<String, BTreeMap<String, String>>, key: Key) {
    match y {
        Yaml::String(s) if key == Key::If && !s.contains("${{") => *s = replace_needs(s, needs),
        Yaml::String(s) => {
            let mut out = String::new();
            let mut rest = s.as_str();

            while let Some(i) = rest.find("${{") {
                out.push_str(&rest[..i]);
                let expr = &rest[i..];
                let end = expr.find("}}").map_or(expr.len(), |e| e + 2);
                out.push_str(&replace_needs(&expr[..end], needs));
                rest = &expr[end..];
            }

            out.push_str(rest);
            *s = out;
        }
        Yaml::Array(items) => items
            .iter_mut()
            .for_each(|i| substitute(i, needs, Key::Other)),
        Yaml::Hash(h) => h
            .iter_mut()
            .for_each(|(k, v)| substitute(v, needs, Key::of(k))),
        _ => {}
    }
}

/// An expression that still reads the `needs` context, such as `toJSON(needs)`
fn reads_needs(y: &Yaml, key: Key) -> Option<String> {
    let mentions = |expr: &str| {
        expr.match_indices("needs").any(|(i, _)| {
            let before = expr[..i].bytes().last();
            let after = expr[i + 5..].bytes().next();
            before.is_none_or(|b| !(b.is_ascii_alphanumeric() || b"_-.".contains(&b)))
                && after.is_none_or(|b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        })
    };

    match y {
        Yaml::String(s) => {
            if key == Key::If && !s.contains("${{") && mentions(s) {
                return Some(s.clone());
            }

            s.split("${{")
                .skip(1)
                .map(|part| part.split("}}").next().unwrap_or(part))
                .find(|expr| mentions(expr))
                .map(|expr| expr.trim().to_string())
        }
        Yaml::Array(items) => items.iter().find_map(|i| reads_needs(i, Key::Other)),
        Yaml::Hash(h) => h.iter().find_map(|(k, v)| reads_needs(v, Key::of(k))),
        _ => None,
    }
}

/// Whether a job's checkout reaches the engine through file sharing or sits in a VM of its own
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Checkout {
    Shared,
    InVm,
}

/**
 * The prepared workflow cut down to the request's job, with a new nonce written into its checkout.
 * act reads its dotfiles from the checkout, so a commit could change how its own run behaves, and
 * they go first
 **/
pub(super) fn isolated(
    request: &RunRequest<'_>,
    tag: &str,
    checkout: Checkout,
) -> Result<String, String> {
    let run_dir = request.run_dir;

    for dotfile in [".actrc", ".env", ".secrets", ".vars", ".input"] {
        let _ = fs::remove_file(run_dir.join(dotfile));
    }

    let prepared = fs::read_to_string(request.workflow).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let nonce = cache::hex(
        &Sha256::digest(format!(
            "{}{}{}",
            run_dir.display(),
            now.as_nanos(),
            std::process::id()
        ))[..16],
    );
    // A new VM has no file sharing to show stale files, so only a shared checkout gets the nonce
    if checkout == Checkout::Shared {
        write_nonces(run_dir, &nonce).map_err(|e| e.to_string())?;
    }

    isolate(
        &prepared,
        request.file,
        request.job,
        request.needs,
        tag,
        (checkout == Checkout::Shared).then_some(nonce.as_str()),
    )
}

/**
 * act runs a job's `needs` along with it, so each job's copy holds only that job, without `needs`,
 * and the outputs and results of its needs become literals. The workflow name gets the checkout's
 * tag, since act names containers after workflow and job and removes an older one of that name
 **/
const NONCE_FILE: &str = "shipcheck-nonce";
/// The same nonce at the checkout's root, a second path the engine has to show fresh
const ROOT_NONCE: &str = ".shipcheck-nonce";

/// Writes the nonce into `.git` and the root, kept out of `git status` by `.git/info/exclude`
fn write_nonces(run_dir: &Path, nonce: &str) -> std::io::Result<()> {
    let git = run_dir.join(".git");
    fs::write(git.join(NONCE_FILE), nonce)?;
    fs::create_dir_all(git.join("info"))?;
    let mut exclude = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(git.join("info/exclude"))?;
    std::io::Write::write_all(&mut exclude, format!("\n/{ROOT_NONCE}\n").as_bytes())?;
    // The tree can hold this name, even as a link to a host file, so it goes first
    let root = run_dir.join(ROOT_NONCE);

    if fs::symlink_metadata(&root).is_ok() {
        fs::remove_file(&root)?;
    }

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root)?;
    std::io::Write::write_all(&mut file, nonce.as_bytes())
}

/**
 * The first step of every job. `OrbStack` has shown a job the files a previous run left at the same
 * bind mount path, and colima shows an empty directory for a path it does not share. Either would
 * let a job pass on a tree it never tested, so a job that does not see this attempt's nonce, in
 * `.git` and at the root, fails
 **/
fn nonce_step(nonce: &str) -> Yaml {
    let mut step = step(
        "shipcheck checkout",
        None,
        format!(
            r#"f="$GITHUB_WORKSPACE/.git/{NONCE_FILE}"
r="$GITHUB_WORKSPACE/{ROOT_NONCE}"
if [ "$(cat "$f" 2>/dev/null)" != {nonce} ] || [ "$(cat "$r" 2>/dev/null)" != {nonce} ]; then
  if [ -e "$f" ] || [ -e "$r" ]; then
    seen="an older checkout"
  else
    seen="no checkout"
  fi
  echo "::error::this job does not see the checkout shipcheck made for it, it sees $seen. The Docker engine shows stale or missing files"
  exit 1
fi
"#
        ),
    );

    if let Yaml::Hash(h) = &mut step {
        h.insert(key("shell"), key("sh"));
    }

    step
}

fn isolate(
    source: &str,
    file: &str,
    job: &str,
    needs: &BTreeMap<String, BTreeMap<String, String>>,
    tag: &str,
    nonce: Option<&str>,
) -> Result<String, String> {
    let mut root = workflow::load(source)?;
    let jobs = workflow::jobs_mut(&mut root)?;
    let mut this = jobs
        .remove(&key(job))
        .ok_or_else(|| format!("workflow has no job {job}"))?;

    if let Yaml::Hash(h) = &mut this {
        h.remove(&key("needs"));
    }

    // A job that needed a matrix job reads it by its own name, which now stands for its legs
    let mut known = needs.clone();

    for (id, outputs) in needs {
        if let Some((base, _)) = id.split_once("__") {
            known
                .entry(base.to_string())
                .or_default()
                .extend(outputs.clone());
        }
    }

    let before = this["env"].clone();
    substitute(&mut this, &known, Key::Other);
    // act evaluates a job's `env` again in each step, which would read a `${{ }}` inside a value as
    // an expression. So the values from needs go to each step's `env`, which act evaluates once
    let refuse = || {
        format!(
            "job {job} passes needs through an env that is not a mapping, which act would evaluate twice"
        )
    };
    let moved: Hash = match (&this["env"], &before) {
        (Yaml::Hash(after), Yaml::Hash(before)) => after
            .iter()
            .filter(|(k, v)| before.get(k) != Some(v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        (after, before) if after == before => Hash::new(),
        _ => return Err(refuse()),
    };

    if !moved.is_empty() {
        let Some(Yaml::Array(steps)) = this.as_mut_hash().and_then(|h| h.get_mut(&key("steps")))
        else {
            return Err(refuse());
        };

        for step in steps.iter_mut() {
            let Yaml::Hash(step) = step else {
                return Err(refuse());
            };
            let env = step.entry(key("env")).or_insert(Yaml::Null);

            if env.is_null() {
                *env = Yaml::Hash(Hash::new());
            }

            let Yaml::Hash(env) = env else {
                return Err(refuse());
            };

            for (k, v) in &moved {
                env.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        if let Some(Yaml::Hash(env)) = this.as_mut_hash().and_then(|h| h.get_mut(&key("env"))) {
            env.retain(|k, _| !moved.contains_key(k));
        }
    }

    if let Some(expr) = reads_needs(&this, Key::Other) {
        return Err(format!(
            "job {job} reads `needs` in `{expr}`, shipcheck passes only needs.<job>.outputs and needs.<job>.result between jobs"
        ));
    }

    // The tag goes on the job's name, since steps read the workflow's name as `github.workflow`
    if let Yaml::Hash(h) = &mut this {
        let name = match h.get(&key("name")) {
            Some(Yaml::String(n)) => n.clone(),
            _ => job.to_string(),
        };
        h.insert(key("name"), key(&format!("{name} {tag}")));

        if let (Some(nonce), Some(Yaml::Array(steps))) = (nonce, h.get_mut(&key("steps"))) {
            steps.insert(0, nonce_step(nonce));
        }
    }

    *jobs = Hash::from_iter([(key(job), this)]);
    // act would name an unnamed workflow after the path it runs, which is outside the checkout
    root.entry(key("name"))
        .or_insert_with(|| key(&format!("{}/{file}", workflow::WORKFLOWS)));
    workflow::emit(root)
}

/**
 * The value of a `continue-on-error` condition for one matrix leg, for the few forms real workflows
 * use: `endsWith`, `startsWith` or `contains` of a matrix value and a quoted string, `==` or `!=`
 * between them, or `true` and `false`. Anything else is None, and the job's failure stays a failure
 **/
/**
 * The leg's value for a field that is `${{ matrix.<name> }}`, or a chain of them and quoted strings
 * joined by `||`, such as `${{ matrix.os || 'ubuntu-latest' }}`. None for any other expression
 **/
/**
 * act evaluates `runs-on: ${{ matrix.os }}` at random across the legs of a matrix (act 0.2.89):
 * the same workflow skips, runs or misplaces legs from one run to the next. Each leg becomes its own
 * job with a one-entry matrix and a literal `runs-on`, which act runs the same way every time.
 * `needs` on the original job is rewritten to all of its legs
 **/
/**
 * Whether a failed job ended before its first step printed, in act's own setup: an image pull that
 * timed out, a container Docker could not create. That is almost always the machine's failure
 **/
pub fn never_started(log: &str) -> bool {
    !log.contains("| shipcheck-start ")
}

/// Whether act refused the workflow before any job, which it does the same way every attempt
pub fn invalid_workflow(log: &str) -> bool {
    log.lines()
        .any(|l| l.starts_with("Error: workflow is not valid."))
}

/// Whether a platform maps the job's `runs-on`. One that is still an expression may, so it counts
fn runs_here(workflow: &str, job: &str, labels: &BTreeSet<String>) -> bool {
    let Ok(root) = workflow::load(workflow) else {
        return true;
    };
    let runs_on = &Yaml::Hash(root)["jobs"][job]["runs-on"];
    let wanted: Vec<&str> = match runs_on {
        Yaml::String(label) => vec![label.as_str()],
        Yaml::Array(list) => list.iter().filter_map(Yaml::as_str).collect(),
        _ => return true,
    };

    wanted.is_empty()
        || wanted
            .iter()
            .any(|l| l.contains("${{") || labels.contains(*l))
}

/// Jobs act skipped because no platform maps their `runs-on`, named as act prints them
pub(super) fn unsupported_jobs(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains("Skipping unsupported platform"))
        .filter_map(|l| {
            let label = l.split_once('[')?.1.split_once(']')?.0;
            Some(label.rsplit('/').next().unwrap_or(label).trim().to_string())
        })
        .collect()
}

/**
 * Stops act and removes its containers and network when shipcheck dies before act ends, by a
 * signal or a timeout. It reads act's group, then waits: `done` means act ended by itself, and an
 * end of input without it means shipcheck is gone. Its own group, so a signal to shipcheck's spares it
 **/
/**
 * Kills act's group and removes what act left, unless shipcheck writes `done`. After `exited` it
 * only cleans up, since the group is reaped and its id can be another's. It holds the slot's lock
 * until it ends, so no new run reuses the slot's network and mounts while it cleans
 **/
/// What separates a run's act process from another's: its containers, apt volume and network
pub(super) fn run_tag(run_dir: &Path) -> String {
    cache::hex(&Sha256::digest(run_dir.to_string_lossy().as_bytes())[..4])
}

/**
 * An internet job gets a bridge of its own rather than act's default, the host's network, which
 * reaches every port on the host's loopback. A crashed run can leave the name taken, so it is
 * removed first. Jobs with services still get the network act creates for them
 **/
/**
 * Setup actions that prune their cache when the `actions/cache` restore missed, or on every run,
 * lose it each time here, since every restore misses without act's cache server. So their steps in
 * the job and in the local composite actions it uses are rewritten, in this checkout only. Git, and
 * so the tree and the key, never see the change. An action reached through a symlink, or one with
 * YAML aliases, keeps its prune
 **/
/**
 * Sets `prune-cache: false` on setup-uv steps. pnpm/action-setup has no such input and runs
 * `pnpm store prune` after any `run_install`, which empties a store that `node_modules` copies from,
 * so its installs move to steps of their own. Returns the local actions the steps use
 **/
pub mod colima;

#[cfg(test)]
mod tests;
