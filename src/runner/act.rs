use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use yaml_rust2::Yaml;
use yaml_rust2::yaml::Hash;

use super::{Outcome, RunRequest, Runner, gc, shard};
use crate::cache;
use crate::config;
use crate::workflow::{self, key, step};

/// Where per-job tool homes live in the job container, a path in `cache_paths` by default
pub const JOB_HOMES: &str = "/home/runner/.shipcheck";

/// Runs workflows with nektos/act, each job in a Docker container
#[derive(Default)]
pub struct Act {
    config: config::Act,
    network: config::Network,
    shards: config::ShardTests,
}

impl Act {
    pub fn new(config: config::Act) -> Self {
        Act {
            config,
            network: config::Network::None,
            shards: config::ShardTests::default(),
        }
    }

    pub fn with_network(self, network: config::Network) -> Self {
        Act { network, ..self }
    }

    pub fn with_shards(self, shards: config::ShardTests) -> Self {
        Act { shards, ..self }
    }
}

/// The label a job gets when `[act.jobs]` gives it an image of its own
pub fn job_label(job: &str) -> String {
    format!("shipcheck-{job}")
}

impl Runner for Act {
    fn preflight(&self) -> Result<(), String> {
        let out = Command::new("docker")
            .args([
                "version",
                "--format",
                "{{.Server.Platform.Name}}|{{range .Server.Components}}{{.Name}}={{.Version}};{{end}}",
            ])
            .output()
            .map_err(|e| format!("docker: {e}"))?;

        if !out.status.success() {
            return Err(format!(
                "docker version failed, is a Docker engine running? {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        if let Some(problem) = engine_problem(String::from_utf8_lossy(&out.stdout).trim()) {
            return Err(problem);
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

        Ok(version)
    }

    /// Adds the `workflow_dispatch` trigger act runs by, expands matrices, gives jobs named in
    /// `[act.jobs]` their own `runs-on` label, and gives each job its own tool homes
    fn prepare(&self, source: &str, file: &str) -> Result<String, String> {
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
            .map(|p| format!("\"{p}\" "))
            .collect();
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

            if let Some(Yaml::Array(steps)) = job.get_mut(&key("steps")) {
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

    fn run(&self, request: &RunRequest, log_path: &Path) -> Result<Outcome, String> {
        let run_dir = request.run_dir;
        let began = Instant::now();
        let (file, job) = (request.file, request.job);

        // act reads these from the checkout, so a commit could change how its own run behaves
        for dotfile in [".actrc", ".env", ".secrets", ".vars", ".input"] {
            let _ = fs::remove_file(run_dir.join(dotfile));
        }

        let wf_path = request.workflow;
        let prepared = fs::read_to_string(wf_path).map_err(|e| e.to_string())?;
        let tag = run_tag(run_dir);
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
        write_nonces(run_dir, &nonce).map_err(|e| e.to_string())?;
        let isolated = isolate(
            &prepared,
            request.file,
            request.job,
            request.needs,
            &tag,
            &nonce,
        )?;
        let shards = (request.shard && isolated.contains(shard::MARK))
            .then(|| shard::count(self.shards, engine_resources()));

        if let Some(n) = shards {
            shard::write_plugin(run_dir, n)?;
        }

        fs::write(wf_path, keep_caches(run_dir, &isolated)?).map_err(|e| e.to_string())?;

        let log = fs::File::create(log_path).map_err(|e| e.to_string())?;
        let labels: BTreeSet<String> = platforms(&self.config)
            .into_keys()
            .chain(
                self.config
                    .job_images(request.file)
                    .into_keys()
                    .map(|j| job_label(&j)),
            )
            .collect();
        // act skips a job with no image without starting a container, and Docker has only about
        // 30 address pools for bridges, which a matrix of macOS legs would take for nothing
        let network = match self.network {
            config::Network::Internet if runs_here(&isolated, request.job, &labels) => {
                Some(create_network(run_dir, request.timeout)?)
            }
            _ => None,
        };
        let mut cmd = Command::new("act");
        let port = free_port()?;
        let home = crate::run::home();

        for (name, _) in volumes(&self.config, request.repo_id, &tag) {
            gc::stamp(&home, "volumes", &name);
        }

        let volumes = mounts(&self.config, request.repo_id, &tag, run_dir)?;

        cmd.arg("workflow_dispatch")
            .arg("-W")
            .arg(wf_path)
            .arg("-j")
            .arg(request.job)
            .arg("--bind")
            .arg("--pull=false")
            // act leaves the container of a failed job running without it
            .arg("--rm")
            // Jobs pass build output to each other through upload-artifact, which act serves only
            // from a directory it is given, one per workflow run
            .arg("--artifact-server-path")
            .arg(request.artifacts)
            // act's fixed default port would let two concurrent runs share one server
            .arg("--artifact-server-port")
            .arg(port.to_string())
            // act binds the LAN address by default, which anyone on the network could use
            .arg("--artifact-server-addr")
            .arg(artifact_addr(network.as_ref()))
            .current_dir(run_dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);

        if !volumes.is_empty() {
            cmd.arg("--container-options").arg(volumes.join(" "));
        }

        for (label, image) in platforms(&self.config) {
            cmd.arg("-P").arg(format!("{label}={image}"));
        }

        for (job, image) in self.config.job_images(request.file) {
            cmd.arg("-P").arg(format!("{}={image}", job_label(&job)));
        }

        // On a Mac the secrets live in the keychain, so act reads a private copy that lives as long as it
        let secrets = crate::secret::read(&crate::run::secrets_file(request.repo_id))?;
        let secrets_copy = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        fs::write(secrets_copy.path(), secrets.unwrap_or_default()).map_err(|e| e.to_string())?;
        cmd.arg("--secret-file").arg(secrets_copy.path());
        // Parallel act processes cloning one action into one cache corrupt it, so each job has its own
        let actions = actions_dir(Some(request.repo_id))
            .join(request.file)
            .join(request.job);

        // Offline, act still clones a missing action but no longer fetches a cached one, which
        // would fail the run when GitHub does not answer. A damaged clone fails offline, so it fetches
        if cached_actions_resolve(&actions) {
            cmd.arg("--action-offline-mode");
        }

        cmd.arg("--action-cache-path").arg(actions);
        // act's default is the host's network, which reaches every service on the machine and the LAN
        cmd.arg("--network")
            .arg(network.as_ref().map_or("none", |n| n.name.as_str()));

        // Job code would get the host through the Docker socket: every file, the App key, other runs
        cmd.arg("--container-daemon-socket").arg("-");
        // act's cache server listens on the LAN with no auth and is shared by every branch, so a
        // stranger or another commit could plant what a later run restores. Warm checkouts replace it
        cmd.arg("--no-cache-server");
        // act fills an unset GITHUB_TOKEN secret from the host's token, so it is set, empty
        cmd.arg("-s").arg("GITHUB_TOKEN=");
        // act also passes a host GITHUB_TOKEN to jobs, so act sees only what it needs
        cmd.env_clear().envs(std::env::vars().filter(|(k, _)| {
            [
                "PATH",
                "HOME",
                "USER",
                "TMPDIR",
                "LANG",
                "LC_ALL",
                "XDG_CACHE_HOME",
            ]
            .contains(&k.as_str())
                || k.starts_with("DOCKER_")
        }));

        // On a Mac, Docker runs in a VM whose 127.0.0.1 is not the Mac's, and act takes the URL it
        // gives jobs from this variable, so jobs reach the loopback artifact server through Docker's route
        if cfg!(target_os = "macos") {
            cmd.env(
                "ACTIONS_RUNTIME_URL",
                format!("http://host.docker.internal:{port}/"),
            );
        }

        for flag in ["--env-file", "--var-file", "--input-file"] {
            cmd.arg(flag).arg("/dev/null");
        }

        chown_tree(run_dir, Some(RUNNER_ID.into()));

        crate::run::timing(file, job, "act prepare", began);
        let spawned = Instant::now();
        let mut reaper = reaper(run_dir);
        // A group of its own, so the reaper can stop act and the docker commands it runs
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| format!("act: {e}"))?;

        if let Some(stdin) = reaper.as_mut().and_then(|r| r.stdin.as_mut()) {
            let _ = writeln!(stdin, "{}", child.id());
        }

        let deadline = Instant::now() + request.timeout;
        let timed = std::env::var_os("SHIPCHECK_TIMINGS").is_some();
        // Log lines that end each phase of act's start, printed once each
        let mut marks = vec![
            ("act until job set up starts", "⭐ Run Set up job"),
            ("act until container up", "🐳  docker exec"),
            ("act until first job step", "shipcheck-start"),
            ("act until job end", "shipcheck-end"),
        ];

        let exit = loop {
            if timed && !marks.is_empty() {
                let text =
                    String::from_utf8_lossy(&fs::read(log_path).unwrap_or_default()).into_owned();
                marks.retain(|(phase, line)| {
                    let seen = text.contains(line);

                    if seen {
                        crate::run::timing(file, job, phase, spawned);
                    }

                    !seen
                });
            }

            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                crate::run::timing(file, job, "act total", spawned);

                // act ended by itself and removed its containers, so the reaper has nothing to do
                if let Some(stdin) = reaper.as_mut().and_then(|r| r.stdin.as_mut()) {
                    let _ = writeln!(stdin, "done");
                }

                break status.code();
            }

            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }

            std::thread::sleep(Duration::from_millis(if timed { 50 } else { 200 }));
        };

        // Without `done`, as after a timeout, it stops act and removes what act left
        if let Some(mut reaper) = reaper {
            drop(reaper.stdin.take());
            let _ = reaper.wait();
        }

        let at = Instant::now();
        hand_back(run_dir);
        crate::run::timing(file, job, "hand_back", at);
        let log = fs::read(log_path).map_err(|e| e.to_string())?;
        let log = String::from_utf8_lossy(&log);

        for image in gc::images_in_log(&log) {
            gc::stamp(&home, "images", &image);
        }

        Ok(Outcome {
            exit,
            unsupported_jobs: unsupported_jobs(&log),
            outputs: read_outputs(&log),
            shards,
            summary: read_summary(&log),
        })
    }

    /// A dead run's containers still mount its slot's directory, and holding the slot's lock proves
    /// the run is dead
    fn reclaim(&self, run_dir: &Path) {
        for path in mount_paths(run_dir) {
            // Both filters must match, so only act's own job containers can ever be removed
            let Ok(out) = Command::new("docker")
                .args(["ps", "-aq", "--filter", "name=^act-", "--filter"])
                .arg(format!("volume={}", path.display()))
                .output()
            else {
                continue;
            };

            for id in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                // `-v` takes the anonymous `.venv` volume along, named cache volumes stay
                let _ = Command::new("docker").args(["rm", "-f", "-v", id]).output();
            }
        }

        gc::remove_env_volumes(&gc::Docker, &run_tag(run_dir));
        // Only after its containers, since Docker keeps a network that one still uses
        let _ = Command::new("docker")
            .args(["network", "rm", &network_name(run_dir)])
            .output();
        hand_back(run_dir);
    }

    fn prune(&self) -> Vec<String> {
        let now = std::time::SystemTime::now();
        gc::prune(
            &gc::Docker,
            &crate::run::home(),
            &home_prefix(),
            gc::OLD_PINS,
            now,
        )
    }

    fn clean(&self, repo_id: Option<&str>) -> Vec<String> {
        let mut done = Vec::new();

        for dir in [cache_dir(repo_id), actions_dir(repo_id)] {
            if dir.exists() && fs::remove_dir_all(&dir).is_ok() {
                done.push(format!("removed {}", dir.display()));
            }
        }

        let prefix = repo_id.map(volume_prefix).unwrap_or_else(home_prefix);
        done.extend(gc::clean(&gc::Docker, &crate::run::home(), &prefix));
        // Networks are per slot, not per repository, and Docker refuses to remove one in use
        let prefix = format!("{}net-", home_prefix());
        let Ok(listed) = Command::new("docker")
            .args(["network", "ls", "--format", "{{.Name}}", "--filter"])
            .arg(format!("name={prefix}"))
            .output()
        else {
            return done;
        };

        let listed = String::from_utf8_lossy(&listed.stdout);
        let removed = listed
            .split_whitespace()
            .filter(|n| n.starts_with(&prefix))
            .filter(|n| {
                Command::new("docker")
                    .args(["network", "rm", n])
                    .output()
                    .is_ok_and(|o| o.status.success())
            })
            .map(|n| format!("removed network {n}"));
        done.extend(removed);
        done
    }
}

/// The image for each label: the defaults below, then `[act] platforms` over them
pub fn platforms(config: &config::Act) -> BTreeMap<String, String> {
    DEFAULT_PLATFORMS
        .into_iter()
        .map(|(l, i)| (l.to_string(), i.to_string()))
        .chain(config.platforms.clone())
        .collect()
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

/// Gives a checkout back to the user after a job, see `chown_tree`
fn hand_back(dir: &Path) {
    let id = |flag: &str| {
        Command::new("id")
            .arg(flag)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    chown_tree(dir, id("-u").zip(id("-g")).map(|(u, g)| format!("{u}:{g}")));
}

/**
 * On Linux a bind mount keeps the host's uids, so the job's `runner` cannot write a checkout the
 * user owns, and the user cannot empty, warm or clean the files the job wrote. A throwaway
 * container hands the tree over before the job and back after it. Docker Desktop on a Mac maps
 * ownership already
 **/
fn chown_tree(dir: &Path, owner: Option<String>) {
    let Some(owner) = owner.filter(|_| cfg!(target_os = "linux") && dir.exists()) else {
        return;
    };

    gc::stamp(&crate::run::home(), "images", BUSYBOX);
    let _ = Command::new("docker")
        .args(["run", "--rm", "--network", "none", "-v"])
        .arg(format!("{}:/w", dir.display()))
        .args([BUSYBOX, "chown", "-R", &owner, "/w"])
        .output();
}

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
    let dir = crate::run::home().join("actions");
    repo_id.map_or(dir.clone(), |id| dir.join(id))
}

/// Whether every action act cloned into `dir`, one `<uses>@<ref>` folder each, still resolves its ref
fn cached_actions_resolve(dir: &Path) -> bool {
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
    let dir = crate::run::home().join("actcache");
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
        script.push_str(&format!(
            "printf 'shipcheck-output %s %s\\n' '{name}' \"$(printf %s \"${var}\" | od -An -v -tx1 | tr -d ' \\n')\"\n"
        ));
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
fn read_outputs(log: &str) -> BTreeMap<String, String> {
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
fn read_summary(log: &str) -> String {
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
                json.push_str(&format!("\\u{unit:04x}"));
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

/// Replaces needs only inside `${{ }}` and in a bare `if`, the places GitHub reads them. Elsewhere a
/// value would be plain text where act still expands any `${{ }}` the value holds
fn substitute(y: &mut Yaml, needs: &BTreeMap<String, BTreeMap<String, String>>, is_if: bool) {
    match y {
        Yaml::String(s) if is_if && !s.contains("${{") => *s = replace_needs(s, needs),
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
        Yaml::Array(items) => items.iter_mut().for_each(|i| substitute(i, needs, false)),
        Yaml::Hash(h) => h
            .iter_mut()
            .for_each(|(k, v)| substitute(v, needs, k.as_str() == Some("if"))),
        _ => {}
    }
}

/// An expression that still reads the `needs` context, such as `toJSON(needs)`
fn reads_needs(y: &Yaml, is_if: bool) -> Option<String> {
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
            if is_if && !s.contains("${{") && mentions(s) {
                return Some(s.clone());
            }

            s.split("${{")
                .skip(1)
                .map(|part| part.split("}}").next().unwrap_or(part))
                .find(|expr| mentions(expr))
                .map(|expr| expr.trim().to_string())
        }
        Yaml::Array(items) => items.iter().find_map(|i| reads_needs(i, false)),
        Yaml::Hash(h) => h
            .iter()
            .find_map(|(k, v)| reads_needs(v, k.as_str() == Some("if"))),
        _ => None,
    }
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
 * The first step of every job. OrbStack has shown a job the files a previous run left at the same
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
    nonce: &str,
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
    substitute(&mut this, &known, false);
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

    if let Some(expr) = reads_needs(&this, false) {
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

        if let Some(Yaml::Array(steps)) = h.get_mut(&key("steps")) {
            steps.insert(0, nonce_step(nonce));
        }
    }

    *jobs = Hash::from_iter([(key(job), this)]);
    // act would name an unnamed workflow after the path it runs, which is outside the checkout
    root.entry(key("name"))
        .or_insert_with(|| key(&format!("{}/{file}", crate::run::WORKFLOWS)));
    workflow::emit(root)
}

fn matches(combo: &Hash, filter: &Hash) -> bool {
    filter.iter().all(|(k, v)| combo.get(k) == Some(v))
}

/// The combinations of a static matrix by GitHub's rules, or None when part of it is an expression
fn legs(matrix: &Hash) -> Option<Vec<Hash>> {
    let (include, exclude) = (key("include"), key("exclude"));
    let mut combos = vec![Hash::new()];
    let mut axes = 0;

    for (k, v) in matrix
        .iter()
        .filter(|(k, _)| **k != include && **k != exclude)
    {
        let Yaml::Array(values) = v else {
            return None;
        };
        axes += 1;
        combos = combos
            .into_iter()
            .flat_map(|c| {
                values.iter().map(move |val| {
                    let mut c = c.clone();
                    c.insert(k.clone(), val.clone());
                    c
                })
            })
            .collect();
    }

    if axes == 0 {
        combos.clear();
    }

    match matrix.get(&exclude) {
        Some(Yaml::Array(filters)) => {
            let filters: Option<Vec<&Hash>> = filters.iter().map(|f| f.as_hash()).collect();
            combos.retain(|c| {
                !filters
                    .as_ref()
                    .is_some_and(|fs| fs.iter().any(|f| matches(c, f)))
            });
        }
        Some(_) => {
            return None;
        }
        None => {}
    }

    let originals = combos.clone();

    match matrix.get(&include) {
        Some(Yaml::Array(entries)) => {
            for entry in entries {
                let entry = entry.as_hash()?;
                let mut added = false;

                // An entry extends every original combination whose own values it does not overwrite
                for (combo, original) in combos.iter_mut().zip(&originals) {
                    if entry
                        .iter()
                        .all(|(k, v)| original.get(k).is_none_or(|o| o == v))
                    {
                        combo.extend(entry.iter().map(|(k, v)| (k.clone(), v.clone())));
                        added = true;
                    }
                }

                if !added {
                    combos.push(entry.clone());
                }
            }
        }
        Some(_) => {
            return None;
        }
        None => {}
    }

    Some(combos)
}

/**
 * The value of a `continue-on-error` condition for one matrix leg, for the few forms real workflows
 * use: `endsWith`, `startsWith` or `contains` of a matrix value and a quoted string, `==` or `!=`
 * between them, or `true` and `false`. Anything else is None, and the job's failure stays a failure
 **/
fn matrix_condition(value: &str, leg: &Hash) -> Option<bool> {
    let expr = value.trim().strip_prefix("${{")?.strip_suffix("}}")?.trim();
    let operand = |text: &str| -> Option<String> {
        let text = text.trim();

        if let Some(quoted) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
            return Some(quoted.replace("''", "'"));
        }

        match leg.get(&key(text.strip_prefix("matrix.")?))? {
            Yaml::String(s) => Some(s.clone()),
            Yaml::Integer(n) => Some(n.to_string()),
            Yaml::Real(r) => Some(r.clone()),
            Yaml::Boolean(b) => Some(b.to_string()),
            _ => None,
        }
    };

    match expr {
        "true" => return Some(true),
        "false" => return Some(false),
        _ => {}
    }

    for (name, test) in [
        (
            "endsWith",
            (|a: &str, b: &str| a.ends_with(b)) as fn(&str, &str) -> bool,
        ),
        ("startsWith", |a, b| a.starts_with(b)),
        ("contains", |a, b| a.contains(b)),
    ] {
        if let Some(args) = expr
            .strip_prefix(name)
            .and_then(|r| r.trim().strip_prefix('('))
            .and_then(|r| r.strip_suffix(')'))
        {
            let (a, b) = args.split_once(',')?;
            return Some(test(&operand(a)?, &operand(b)?));
        }
    }

    if let Some((a, b)) = expr.split_once("!=") {
        return Some(operand(a)? != operand(b)?);
    }

    let (a, b) = expr.split_once("==")?;
    Some(operand(a)? == operand(b)?)
}

/**
 * The leg's value for a field that is `${{ matrix.<name> }}`, or a chain of them and quoted strings
 * joined by `||`, such as `${{ matrix.os || 'ubuntu-latest' }}`. None for any other expression
 **/
fn resolve_matrix(value: &Yaml, leg: &Hash) -> Option<Yaml> {
    let inner = value
        .as_str()?
        .trim()
        .strip_prefix("${{")?
        .strip_suffix("}}")?
        .trim();

    let operands: Vec<&str> = inner.split("||").map(str::trim).collect();

    for (n, operand) in operands.iter().enumerate() {
        let last = n + 1 == operands.len();
        let value = match operand
            .strip_prefix('\'')
            .and_then(|t| t.strip_suffix('\''))
        {
            Some(quoted) if quoted.contains('\'') => return None,
            Some(quoted) => Some(key(quoted)),
            None => {
                let name = operand.strip_prefix("matrix.")?;

                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
                {
                    return None;
                }

                leg.get(&key(name)).cloned()
            }
        };
        let falsy = match &value {
            None | Some(Yaml::Null) | Some(Yaml::Boolean(false)) | Some(Yaml::Integer(0)) => true,
            Some(Yaml::String(s)) => s.is_empty(),
            _ => false,
        };

        // As in GitHub, `a || b` is `a` unless it is falsy, and the last operand whatever it is
        if last || !falsy {
            return value;
        }
    }

    None
}

/**
 * act evaluates `runs-on: ${{ matrix.os }}` at random across the legs of a matrix (act 0.2.89):
 * the same workflow skips, runs or misplaces legs from one run to the next. Each leg becomes its own
 * job with a one-entry matrix and a literal `runs-on`, which act runs the same way every time.
 * `needs` on the original job is rewritten to all of its legs
 **/
fn expand_matrices(jobs: Hash) -> Hash {
    let mut out = Hash::new();
    let mut renamed: BTreeMap<String, Vec<Yaml>> = BTreeMap::new();

    for (id, job) in jobs {
        let legs = job["strategy"]["matrix"]
            .as_hash()
            .and_then(legs)
            .filter(|l| !l.is_empty());
        let (Some(legs), Some(name)) = (legs, id.as_str().map(str::to_string)) else {
            out.insert(id, job);
            continue;
        };

        for (n, leg) in legs.into_iter().enumerate() {
            let mut leg_job = job.clone();
            let Yaml::Hash(h) = &mut leg_job else {
                continue;
            };

            // shipcheck reads `continue-on-error` itself to judge the workflow, so it must be literal too
            for field in ["runs-on", "continue-on-error", "container"] {
                if let Some(resolved) = h.get(&key(field)).and_then(|r| resolve_matrix(r, &leg)) {
                    h.insert(key(field), resolved);
                }
            }

            if let Some(allowed) = h
                .get(&key("continue-on-error"))
                .and_then(|v| v.as_str())
                .and_then(|v| matrix_condition(v, &leg))
            {
                h.insert(key("continue-on-error"), Yaml::Boolean(allowed));
            }

            if let Some(Yaml::Hash(strategy)) = h.get_mut(&key("strategy")) {
                strategy.insert(
                    key("matrix"),
                    Yaml::Hash(Hash::from_iter([(
                        key("include"),
                        Yaml::Array(vec![Yaml::Hash(leg)]),
                    )])),
                );
            }

            let leg_id = key(&format!("{name}__{}", n + 1));
            renamed
                .entry(name.clone())
                .or_default()
                .push(leg_id.clone());
            out.insert(leg_id, leg_job);
        }
    }

    for job in out.values_mut() {
        let Yaml::Hash(job) = job else {
            continue;
        };
        let rename = |n: &Yaml| match n.as_str().and_then(|s| renamed.get(s)) {
            Some(ids) => ids.clone(),
            None => vec![n.clone()],
        };
        let needs = match job.get(&key("needs")) {
            Some(Yaml::Array(list)) => Some(Yaml::Array(list.iter().flat_map(rename).collect())),
            Some(single @ Yaml::String(_)) => Some(Yaml::Array(rename(single))),
            _ => None,
        };

        if let Some(needs) = needs {
            job.insert(key("needs"), needs);
        }
    }

    out
}

/**
 * Whether a failed job ended before its first step printed, in act's own setup: an image pull that
 * timed out, a container Docker could not create. That is almost always the machine's failure
 **/
pub fn never_started(log: &str) -> bool {
    !log.contains("| shipcheck-start ")
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
fn unsupported_jobs(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains("Skipping unsupported platform"))
        .filter_map(|l| {
            let label = l.split_once('[')?.1.split_once(']')?.0;
            Some(label.rsplit('/').next().unwrap_or(label).trim().to_string())
        })
        .collect()
}

/// The paths act's containers mount a run's directory at. Docker matches the exact path, never a
/// parent, and act mounts the one its working directory resolves to, /private/var for /var on macOS
fn mount_paths(run_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![run_dir.to_path_buf()];

    if let (Some(parent), Some(name)) = (run_dir.parent(), run_dir.file_name())
        && let Ok(real) = parent.canonicalize()
    {
        paths.push(real.join(name));
    }

    paths
}

/**
 * Stops act and removes its containers and network when shipcheck dies before act ends, by a
 * signal or a timeout. It reads act's group, then waits: `done` means act ended by itself, and an
 * end of input without it means shipcheck is gone. Its own group, so a signal to shipcheck's spares it
 **/
const REAPER: &str = r#"read -r pgid || exit 0
read -r done && exit 0
kill -KILL -- "-$pgid" 2>/dev/null
net=$1
shift
for p in "$@"; do
  docker ps -aq --filter name=^act- --filter "volume=$p" | while read -r id; do docker rm -f -v "$id"; done
done
docker network rm "$net"
"#;

fn reaper(run_dir: &Path) -> Option<std::process::Child> {
    Command::new("sh")
        .args(["-c", REAPER, "sh", &network_name(run_dir)])
        .args(mount_paths(run_dir))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()
}

/// What separates a run's act process from another's: its containers, apt volume and network
fn run_tag(run_dir: &Path) -> String {
    cache::hex(&Sha256::digest(run_dir.to_string_lossy().as_bytes())[..4])
}

fn network_name(run_dir: &Path) -> String {
    format!("{}net-{}", home_prefix(), run_tag(run_dir))
}

/// A Docker network of the run's own, which `reclaim` removes
struct Network {
    name: String,
    gateway: std::net::Ipv4Addr,
}

/**
 * An internet job gets a bridge of its own rather than act's default, the host's network, which
 * reaches every port on the host's loopback. A crashed run can leave the name taken, so it is
 * removed first. Jobs with services still get the network act creates for them
 **/
fn create_network(run_dir: &Path, wait: Duration) -> Result<Network, String> {
    let name = network_name(run_dir);
    let _ = Command::new("docker")
        .args(["network", "rm", &name])
        .output();
    let deadline = Instant::now() + wait.min(POOL_WAIT);
    until_pooled(deadline, Duration::from_secs(5), || {
        let out = Command::new("docker")
            .args([
                "network",
                "create",
                "--driver",
                "bridge",
                "--label",
                "shipcheck",
                &name,
            ])
            .output()
            .map_err(|e| format!("docker network create: {e}"))?;

        if out.status.success() {
            return Ok(());
        }

        Err(format!(
            "docker network create {name}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    })?;

    let out = Command::new("docker")
        .args(["network", "inspect", &name, "-f"])
        .arg("{{range .IPAM.Config}}{{.Gateway}} {{end}}")
        .output()
        .map_err(|e| format!("docker network inspect: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let gateway = gateway(&text)
        .ok_or_else(|| format!("network {name} has no private IPv4 gateway: {text}"))?;
    Ok(Network { name, gateway })
}

/// How long a job waits for a Docker address pool before it fails
const POOL_WAIT: Duration = Duration::from_secs(10 * 60);

/// Calls `create` until it succeeds, fails for another reason than a lack of address pools, or the
/// deadline passes. Other runs' jobs hold every pool, and each gives its pool back when it ends
fn until_pooled(
    deadline: Instant,
    pause: Duration,
    mut create: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    loop {
        match create() {
            Err(e) if pools_taken(&e) && Instant::now() < deadline => std::thread::sleep(pause),
            done => return done,
        }
    }
}

fn pools_taken(err: &str) -> bool {
    err.contains("all predefined address pools have been fully subnetted")
}

fn gateway(inspect: &str) -> Option<std::net::Ipv4Addr> {
    inspect
        .split_whitespace()
        .find_map(|a| a.parse::<std::net::Ipv4Addr>().ok())
        .filter(|a| a.is_private())
}

/// A bridge container on Linux reaches the host only at its network's gateway, never the host's
/// loopback. act gives jobs this address too, unless ACTIONS_RUNTIME_URL is set as on a Mac
fn artifact_addr(network: Option<&Network>) -> String {
    match network {
        Some(n) if !cfg!(target_os = "macos") => n.gateway.to_string(),
        _ => "127.0.0.1".into(),
    }
}

// ponytail: another process can take the port between this check and act binding it, retry the run if so
fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("no free port for the artifact server: {e}"))
}

fn docker_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// The start of every volume name of one repo. The hash keeps `a/b` and `a/b-c` apart, whose
/// readable parts both turn into `a-b`
fn volume_prefix(repo_id: &str) -> String {
    let hash = cache::hex(&Sha256::digest(repo_id.as_bytes())[..4]);
    format!("{}{}-{hash}-", home_prefix(), docker_safe(repo_id))
}

/// The start of every volume name under this `SHIPCHECK_HOME`, so `clean --all` of one home leaves
/// the volumes of another, such as a test's, alone
pub fn home_prefix() -> String {
    let home = crate::run::home();
    let hash = cache::hex(&Sha256::digest(home.to_string_lossy().as_bytes())[..3]);
    format!("shipcheck-{hash}-")
}

/**
 * Setup actions that prune their cache when the `actions/cache` restore missed, or on every run,
 * lose it each time here, since every restore misses without act's cache server. So their steps in
 * the job and in the local composite actions it uses are rewritten, in this checkout only. Git, and
 * so the tree and the key, never see the change. An action reached through a symlink, or one with
 * YAML aliases, keeps its prune
 **/
fn keep_caches(run_dir: &Path, workflow: &str) -> Result<String, String> {
    let mut root = workflow::load(workflow)?;
    let before = root.clone();
    let mut actions = Vec::new();

    for job in workflow::jobs_mut(&mut root)?.values_mut() {
        if let Some(Yaml::Array(steps)) = job.as_mut_hash().and_then(|j| j.get_mut(&key("steps"))) {
            actions.extend(no_prune(steps));
        }
    }

    let changed = root != before;

    let mut seen = BTreeSet::new();

    while let Some(action) = actions.pop() {
        let Some(path) = local_action(run_dir, &action) else {
            continue;
        };

        if !seen.insert(path.clone()) {
            continue;
        }

        let source = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;

        // The branch wrote this file, and an alias bomb would expand on the host before any timeout
        if !workflow::safe_to_load(&source) {
            continue;
        }

        let mut action = workflow::load(&source)?;
        let Some(Yaml::Array(steps)) = action
            .get_mut(&key("runs"))
            .and_then(|r| r.as_mut_hash())
            .and_then(|r| r.get_mut(&key("steps")))
        else {
            continue;
        };
        let before = steps.clone();
        actions.extend(no_prune(steps));

        if *steps != before {
            fs::write(&path, workflow::emit(action)?).map_err(|e| e.to_string())?;
        }
    }

    if changed {
        workflow::emit(root)
    } else {
        Ok(workflow.to_string())
    }
}

/**
 * Sets `prune-cache: false` on setup-uv steps. pnpm/action-setup has no such input and runs
 * `pnpm store prune` after any `run_install`, which empties a store that `node_modules` copies from,
 * so its installs move to steps of their own. Returns the local actions the steps use
 **/
fn no_prune(steps: &mut Vec<Yaml>) -> Vec<String> {
    let mut local = Vec::new();
    let mut i = 0;

    while i < steps.len() {
        let Some(step) = steps[i].as_mut_hash() else {
            i += 1;
            continue;
        };
        let uses = step
            .get(&key("uses"))
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string();
        let mut installs = Vec::new();

        if uses.starts_with("./") {
            local.push(uses);
        } else if uses.starts_with("astral-sh/setup-uv@") {
            let with = step
                .entry(key("with"))
                .or_insert_with(|| Yaml::Hash(Hash::new()));

            if let Yaml::Hash(with) = with {
                with.insert(key("prune-cache"), key("false"));
            }
        } else if uses.starts_with("pnpm/action-setup@")
            && let Some(Yaml::Hash(with)) = step.get_mut(&key("with"))
            && let Some(runs) = with.get(&key("run_install")).and_then(pnpm_installs)
        {
            with.remove(&key("run_install"));
            installs = runs
                .into_iter()
                .map(|(run, cwd)| install_step(step, run, cwd))
                .collect();
        }

        i += 1;

        for install in installs {
            steps.insert(i, install);
            i += 1;
        }
    }

    local
}

/// The commands pnpm/action-setup runs for `run_install`, with their `cwd`. None leaves the step
/// alone: no install, an expression, or a value the action itself would refuse
fn pnpm_installs(input: &Yaml) -> Option<Vec<(String, Option<String>)>> {
    let text = match input {
        Yaml::Boolean(b) => b.to_string(),
        Yaml::String(s) if !s.contains("${{") => s.clone(),
        _ => return None,
    };

    if !workflow::safe_to_load(&text) {
        return None;
    }

    let entries = match yaml_rust2::YamlLoader::load_from_str(&text).ok()?.pop()? {
        Yaml::Boolean(true) => vec![Yaml::Hash(Hash::from_iter([(
            key("recursive"),
            Yaml::Boolean(true),
        )]))],
        Yaml::Hash(h) => vec![Yaml::Hash(h)],
        Yaml::Array(a) if !a.is_empty() => a,
        _ => return None,
    };
    let mut runs = Vec::new();

    for entry in entries {
        let (mut recursive, mut cwd, mut args) = (false, None, Vec::new());

        for (k, v) in entry.into_hash()? {
            match (k.as_str()?, v) {
                ("recursive", Yaml::Boolean(b)) => recursive = b,
                ("cwd", Yaml::String(dir)) => cwd = Some(dir),
                ("args", Yaml::Array(a)) => {
                    args = a
                        .iter()
                        .map(|a| a.as_str().map(String::from))
                        .collect::<Option<_>>()?
                }
                _ => return None,
            }
        }

        let cmd = [
            "pnpm",
            if recursive {
                "recursive install"
            } else {
                "install"
            },
        ]
        .into_iter()
        .map(String::from)
        .chain(args);
        runs.push((cmd.collect::<Vec<_>>().join(" "), cwd));
    }

    Some(runs)
}

/// A step that runs one of the setup step's installs, under the setup step's condition and env
fn install_step(setup: &Hash, run: String, cwd: Option<String>) -> Yaml {
    let mut h = Hash::new();
    h.insert(key("name"), key(&run));

    for field in ["if", "env", "continue-on-error"] {
        if let Some(v) = setup.get(&key(field)) {
            h.insert(key(field), v.clone());
        }
    }

    h.insert(key("run"), key(&run));
    // Composite actions need a shell on every run step
    h.insert(key("shell"), key("bash"));

    if let Some(dir) = cwd {
        h.insert(key("working-directory"), key(&dir));
    }

    Yaml::Hash(h)
}

/// The `action.yml` of a `uses: ./dir` action inside the checkout, if no part of its path is a link
fn local_action(run_dir: &Path, uses: &str) -> Option<PathBuf> {
    let rel = Path::new(uses.strip_prefix("./")?);

    if !rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }

    let mut path = run_dir.to_path_buf();

    for part in rel.components() {
        path.push(part);

        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            return None;
        }
    }

    ["action.yml", "action.yaml"]
        .map(|f| path.join(f))
        .into_iter()
        .find(|f| fs::symlink_metadata(f).is_ok_and(|m| m.is_file()))
}

/// A new anonymous volume at the checkout's `.venv`, so a Python venv lives off the macOS bind mount
/// and starts empty each run. act splits its container options as a shell would, hence the quotes
fn venv_mount(run_dir: &Path) -> Result<String, String> {
    // act's workspace is the real path, /private/var on a Mac where the run dir is under /var
    let real = run_dir
        .canonicalize()
        .unwrap_or_else(|_| run_dir.to_path_buf());
    let dir = real.join(".venv");
    let dir = dir
        .to_str()
        .filter(|d| !d.contains(['\'', ',']))
        .ok_or_else(|| {
            format!(
                "{}: a run directory needs a path without quotes or commas",
                run_dir.display()
            )
        })?;
    Ok(format!("--mount 'type=volume,dst={dir}'"))
}

/// The volumes `cache_paths` share per repository, and the ones `slot_paths` get per slot and job
fn volumes<'a>(config: &'a config::Act, repo_id: &str, tag: &str) -> Vec<(String, &'a String)> {
    let shared = config
        .cache_paths
        .iter()
        .map(|p| (volume_name(repo_id, p), p));
    let slot = (config.slot_paths.iter()).map(|p| (volume_name(repo_id, &format!("{tag}{p}")), p));
    shared.chain(slot).collect()
}

/// `cache_paths` share a volume per repository, `slot_paths` get one per slot and job, as `tag` names
fn mounts(
    config: &config::Act,
    repo_id: &str,
    tag: &str,
    run_dir: &Path,
) -> Result<Vec<String>, String> {
    let mut mounts: Vec<String> = volumes(config, repo_id, tag)
        .into_iter()
        .map(|(v, p)| format!("-v {v}:{p}"))
        .collect();
    mounts.push(venv_mount(run_dir)?);
    Ok(mounts)
}

fn volume_name(repo_id: &str, path: &str) -> String {
    format!(
        "{}{}",
        volume_prefix(repo_id),
        docker_safe(path.trim_start_matches('/'))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaml_rust2::YamlLoader;

    fn load(s: &str) -> Yaml {
        YamlLoader::load_from_str(s).unwrap().remove(0)
    }

    fn inject_with(src: &str, nonce: &str, act: config::Act) -> Result<String, String> {
        workflow::add_markers(&Act::new(act).prepare(src, "ci.yml")?, nonce)
    }

    fn inject(src: &str, nonce: &str) -> Result<String, String> {
        inject_with(src, nonce, config::Act::default())
    }

    const CI: &str = "\
name: CI
on:
  push:
    branches: [main]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          cargo build
          cargo test
";

    #[test]
    fn adds_dispatch_to_every_trigger_form() {
        for on in [
            "on: push",
            "on: [push, pull_request]",
            "on:\n  push:\n    branches: [main]",
        ] {
            let out = load(&inject(&format!("{on}\njobs: {{}}\n"), "n").unwrap());
            let on = &out["on"];
            let has = match on {
                Yaml::Hash(h) => h.contains_key(&key("workflow_dispatch")),
                Yaml::Array(a) => a.contains(&key("workflow_dispatch")),
                _ => false,
            };
            assert!(has, "no workflow_dispatch in {on:?}");
        }
    }

    #[test]
    fn the_checkout_step_needs_both_nonces() {
        let script = nonce_step("abc")["run"].as_str().unwrap().to_string();
        let run = |dir: &Path| {
            let out = Command::new("sh")
                .args(["-e", "-c", &script])
                .env("GITHUB_WORKSPACE", dir)
                .output()
                .unwrap();
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(&git).unwrap();

        let (ok, out) = run(dir.path());
        assert!(!ok && out.contains("it sees no checkout"), "{out}");

        fs::write(git.join(NONCE_FILE), "old").unwrap();
        let (ok, out) = run(dir.path());
        assert!(!ok && out.contains("it sees an older checkout"), "{out}");

        fs::write(git.join(NONCE_FILE), "abc").unwrap();
        let (ok, out) = run(dir.path());
        assert!(
            !ok && out.contains("an older checkout"),
            "no root nonce: {out}"
        );

        write_nonces(dir.path(), "abc").unwrap();
        assert_eq!(run(dir.path()), (true, String::new()));
    }

    #[test]
    fn the_root_nonce_replaces_a_planted_link_and_stays_out_of_git() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(outside.path().join("zshrc"), "mine").unwrap();
        std::os::unix::fs::symlink(outside.path().join("zshrc"), dir.path().join(ROOT_NONCE))
            .unwrap();
        write_nonces(dir.path(), "abc").unwrap();
        assert_eq!(
            fs::read_to_string(outside.path().join("zshrc")).unwrap(),
            "mine"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(ROOT_NONCE)).unwrap(),
            "abc"
        );
        let exclude = fs::read_to_string(dir.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("/.shipcheck-nonce\n"));
    }

    #[test]
    fn shards_pytest_only_when_the_config_asks() {
        let wf = "on: push\njobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n      - run: uv run pytest\n";
        let off = Act::default().prepare(wf, "ci.yml").unwrap();
        assert!(
            !off.contains(shard::MARK),
            "a config without shard_tests must keep its key"
        );
        let on = Act::default()
            .with_shards(config::ShardTests::Fixed(3))
            .prepare(wf, "ci.yml")
            .unwrap();
        let step = &load(&on)["jobs"]["t"]["steps"][1];
        assert_eq!(
            step["name"].as_str(),
            Some("Run uv run pytest (shipcheck: 3 shards)")
        );
        assert!(
            !on.contains("nproc"),
            "the shard count must stay out of the key"
        );
    }

    #[test]
    fn keeps_existing_triggers() {
        let out = load(&inject(CI, "n").unwrap());
        assert_eq!(out["on"]["push"]["branches"][0].as_str(), Some("main"));
    }

    #[test]
    fn setup_uv_keeps_its_cache_in_the_checkout_only() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let setup = dir.path().join(".github/actions/setup");
        fs::create_dir_all(&setup).unwrap();
        let composite = "runs:\n  using: composite\n  steps:\n    - uses: astral-sh/setup-uv@v7\n      with:\n        enable-cache: true\n";
        fs::write(setup.join("action.yml"), composite).unwrap();
        fs::write(outside.path().join("action.yml"), composite).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".github/actions/linked"))
            .unwrap();
        let wf = "on: push\njobs:\n  t:\n    steps:\n      - uses: astral-sh/setup-uv@v6\n      - uses: ./.github/actions/setup\n      - uses: ./.github/actions/linked\n      - uses: ./../escape\n";

        let out = load(&keep_caches(dir.path(), wf).unwrap());
        assert_eq!(
            out["jobs"]["t"]["steps"][0]["with"]["prune-cache"].as_str(),
            Some("false")
        );
        let action = load(&fs::read_to_string(setup.join("action.yml")).unwrap());
        let with = &action["runs"]["steps"][0]["with"];
        assert_eq!(with["prune-cache"].as_str(), Some("false"));
        assert_eq!(with["enable-cache"].as_bool(), Some(true));
        assert_eq!(
            fs::read_to_string(outside.path().join("action.yml")).unwrap(),
            composite
        );

        let plain = "on: push\njobs:\n  t:\n    steps:\n      - run: true\n";
        assert_eq!(keep_caches(dir.path(), plain).unwrap(), plain);

        // An action.yml with aliases is left as the branch wrote it, never expanded
        let bomb = "a: &a [x, x]\nb: &b [*a, *a]\nruns:\n  using: composite\n  steps:\n    - uses: astral-sh/setup-uv@v7\n";
        fs::write(setup.join("action.yml"), bomb).unwrap();
        keep_caches(dir.path(), wf).unwrap();
        assert_eq!(fs::read_to_string(setup.join("action.yml")).unwrap(), bomb);
    }

    #[test]
    fn pnpm_installs_move_out_of_the_setup_step() {
        let dir = tempfile::tempdir().unwrap();
        let setup = dir.path().join("setup");
        fs::create_dir_all(&setup).unwrap();
        let composite = "runs:\n  using: composite\n  steps:\n    - uses: pnpm/action-setup@v4\n      with:\n        run_install: true\n";
        fs::write(setup.join("action.yml"), composite).unwrap();
        let bomb = dir.path().join("bomb");
        fs::create_dir_all(&bomb).unwrap();
        let aliased = "a: &a [x]\nruns:\n  using: composite\n  steps:\n    - uses: pnpm/action-setup@v4\n      with: {run_install: true, b: *a}\n";
        fs::write(bomb.join("action.yml"), aliased).unwrap();
        let wf = r#"on: push
jobs:
  t:
    steps:
      - uses: pnpm/action-setup@v4
        if: always()
        env: {CI: "1"}
        with:
          version: 10
          run_install: |
            - args: [--frozen-lockfile]
              recursive: true
            - cwd: web
      - uses: pnpm/action-setup@v4
        with: {run_install: "${{ matrix.install }}"}
      - uses: pnpm/action-setup@v4
        with: {run_install: false}
      - uses: ./setup
      - uses: ./bomb
"#;
        let out = load(&keep_caches(dir.path(), wf).unwrap());
        let steps = out["jobs"]["t"]["steps"].as_vec().unwrap();
        assert_eq!(steps.len(), 7);
        assert!(steps[0]["with"]["run_install"].is_badvalue());
        assert_eq!(steps[0]["with"]["version"].as_i64(), Some(10));
        assert_eq!(
            steps[1]["run"].as_str(),
            Some("pnpm recursive install --frozen-lockfile")
        );
        assert_eq!(steps[1]["if"].as_str(), Some("always()"));
        assert_eq!(steps[1]["env"]["CI"].as_str(), Some("1"));
        assert_eq!(steps[2]["run"].as_str(), Some("pnpm install"));
        assert_eq!(steps[2]["working-directory"].as_str(), Some("web"));
        assert_eq!(
            steps[3]["with"]["run_install"].as_str(),
            Some("${{ matrix.install }}")
        );
        assert_eq!(steps[4]["with"]["run_install"].as_bool(), Some(false));

        let action = load(&fs::read_to_string(setup.join("action.yml")).unwrap());
        let inner = action["runs"]["steps"].as_vec().unwrap();
        assert!(inner[0]["with"]["run_install"].is_badvalue());
        assert_eq!(inner[1]["run"].as_str(), Some("pnpm recursive install"));
        assert_eq!(inner[1]["shell"].as_str(), Some("bash"));
        assert_eq!(
            fs::read_to_string(bomb.join("action.yml")).unwrap(),
            aliased
        );
    }

    #[test]
    fn the_homes_step_owns_the_dirs_docker_made_above_a_mount() {
        use std::os::unix::fs::PermissionsExt;
        let out = load(&inject(CI, "abc").unwrap());
        let dir = tempfile::tempdir().unwrap();
        let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
        let homes = out["jobs"]["test"]["steps"][1]["run"]
            .as_str()
            .unwrap()
            .replace("/home/runner", &home.to_string_lossy());
        // As Docker leaves a volume at ~/.bun/install/cache: the dirs above it are root's
        fs::create_dir_all(home.join(".bun/install/cache")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("sudo"), "#!/bin/sh\nchmod u+w \"$4\"\n").unwrap();
        fs::set_permissions(bin.join("sudo"), fs::Permissions::from_mode(0o755)).unwrap();
        for d in [".bun/install", ".bun"] {
            fs::set_permissions(home.join(d), fs::Permissions::from_mode(0o555)).unwrap();
        }

        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let status = std::process::Command::new("bash")
            .args(["-e", "-c", &homes])
            .env("PATH", path)
            .env("HOME", &home)
            .env("GITHUB_WORKSPACE", dir.path())
            .env("RUNNER_TEMP", dir.path().join("temp"))
            .env("CARGO_HOME", dir.path().join("cargo"))
            .env("GITHUB_PATH", dir.path().join("github_path"))
            .status()
            .unwrap();
        assert!(status.success());
        assert!(fs::create_dir(home.join(".bun/install/bin")).is_ok());
        assert!(fs::create_dir(home.join(".bun/bin")).is_ok());
    }

    #[test]
    fn the_reaper_cleans_up_only_when_shipcheck_dies_first() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("docker.log");
        let docker = dir.path().join("docker");
        // `ps` names one container, every call is logged
        let fake = format!(
            "#!/bin/sh\necho \"$*\" >> {}\n[ \"$1\" = ps ] && echo c1\nexit 0\n",
            log.display()
        );
        fs::write(&docker, fake).unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:/usr/bin:/bin", dir.path().display());

        for ended in [true, false] {
            let _ = fs::remove_file(&log);
            let mut act = Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap();
            let mut reaper = Command::new("sh")
                .args(["-c", REAPER, "sh", "net-1", "/runs/0"])
                .env("PATH", &path)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = reaper.stdin.take().unwrap();
            writeln!(stdin, "{}", act.id()).unwrap();

            if ended {
                writeln!(stdin, "done").unwrap();
            }

            drop(stdin);
            assert!(reaper.wait().unwrap().success());
            let calls = fs::read_to_string(&log).unwrap_or_default();

            if ended {
                assert_eq!(calls, "");
                assert!(act.try_wait().unwrap().is_none(), "act was killed");
                let _ = act.kill();
            } else {
                assert_eq!(
                    calls,
                    "ps -aq --filter name=^act- --filter volume=/runs/0\nrm -f -v c1\nnetwork rm net-1\n"
                );
                assert!(act.wait().unwrap().code().is_none(), "act still runs");
            }

            let _ = act.wait();
        }
    }

    #[test]
    fn puts_homes_between_the_markers() {
        let out = load(&inject(CI, "abc").unwrap());
        let steps = out["jobs"]["test"]["steps"].as_vec().unwrap();
        assert_eq!(steps.len(), 5);
        assert_eq!(
            steps[0]["run"].as_str(),
            Some("echo \"shipcheck-start abc ${{ github.job }}\"")
        );
        let homes = steps[1]["run"].as_str().unwrap();
        assert!(homes.starts_with(
            "for d in \"/home/runner/.shipcheck\" \"/home/runner/.npm\" \"/home/runner/.bun/install/cache\" \"/home/runner/.cache\" \"$GITHUB_WORKSPACE/.venv\"; do\n  while [ -w \"$d\" ] || sudo -n chown"
        ));
        assert!(homes.ends_with(
            "done\nmkdir -p \"$RUNNER_TEMP\" || true\nmkdir -p \"$CARGO_HOME/bin\"\necho \"$CARGO_HOME/bin\" >> \"$GITHUB_PATH\"\n"
        ));
        assert_eq!(
            out["jobs"]["test"]["env"]["RUNNER_TEMP"].as_str(),
            Some("/home/runner/work/_temp")
        );
        assert_eq!(steps[2]["uses"].as_str(), Some("actions/checkout@v4"));
        assert_eq!(steps[3]["run"].as_str(), Some("cargo build\ncargo test\n"));
        assert_eq!(steps[4]["if"].as_str(), Some("always()"));
    }

    #[test]
    fn gives_each_job_its_own_tool_homes() {
        let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - run: cargo build
  b:
    runs-on: ubuntu-latest
    env:
      CARGO_HOME: /custom
    steps:
      - run: cargo test
";
        let out = load(&inject(wf, "n").unwrap());
        assert_eq!(
            out["jobs"]["a"]["env"]["RUSTUP_HOME"].as_str(),
            Some("/home/runner/.shipcheck/a/rustup")
        );
        assert_eq!(
            out["jobs"]["a"]["env"]["CARGO_HOME"].as_str(),
            Some("/home/runner/.shipcheck/a/cargo")
        );
        // A job that sets its own home keeps it
        assert_eq!(
            out["jobs"]["b"]["env"]["CARGO_HOME"].as_str(),
            Some("/custom")
        );
        assert_eq!(
            out["jobs"]["b"]["env"]["RUSTUP_HOME"].as_str(),
            Some("/home/runner/.shipcheck/b/rustup")
        );
    }

    #[test]
    fn takes_tool_homes_from_the_table() {
        let act = config::Act {
            job_homes: BTreeMap::from([(
                "GOMODCACHE".to_string(),
                config::Home::Dir("gomod".into()),
            )]),
            ..Default::default()
        };
        let out = load(&inject_with(CI, "n", act).unwrap());
        let env = &out["jobs"]["test"]["env"];
        assert_eq!(
            env["GOMODCACHE"].as_str(),
            Some("/home/runner/.shipcheck/test/gomod")
        );
        assert!(env["CARGO_HOME"].is_badvalue());
        // No home has a `path`, so the homes step only hands the mounts to the job's user
        let steps = out["jobs"]["test"]["steps"].as_vec().unwrap();
        assert_eq!(steps.len(), 5);
        assert!(
            steps[1]["run"]
                .as_str()
                .unwrap()
                .ends_with("done\nmkdir -p \"$RUNNER_TEMP\" || true\n")
        );
        assert_eq!(steps[2]["uses"].as_str(), Some("actions/checkout@v4"));
    }

    #[test]
    fn an_empty_homes_table_adds_nothing() {
        let act = config::Act {
            job_homes: BTreeMap::new(),
            ..Default::default()
        };
        let out = load(&inject_with(CI, "n", act).unwrap());
        let env = out["jobs"]["test"]["env"].as_hash().unwrap();
        assert!(env.keys().all(|k| {
            let k = k.as_str().unwrap();
            k.starts_with("GIT_CONFIG_") || k == "RUNNER_TEMP"
        }));
        assert_eq!(out["jobs"]["test"]["steps"].as_vec().unwrap().len(), 5);
    }

    #[test]
    fn container_jobs_and_mapped_labels_keep_their_own() {
        let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    container: node:22
    steps:
      - run: npm test
";
        let out = load(&inject(wf, "n").unwrap());
        assert!(out["jobs"]["a"]["env"]["RUNNER_TEMP"].is_badvalue());
        assert_eq!(out["jobs"]["a"]["runs-on"].as_str(), Some("ubuntu-24.04"));
        let act = config::Act {
            platforms: BTreeMap::from([("ubuntu-latest".into(), "img@sha256:1".into())]),
            ..Default::default()
        };
        let out = load(&inject_with(wf, "n", act).unwrap());
        assert_eq!(out["jobs"]["a"]["runs-on"].as_str(), Some("ubuntu-latest"));
    }

    #[test]
    fn the_key_sees_the_default_images() {
        let mapped = config::Act {
            platforms: BTreeMap::from([("ubuntu-22.04".into(), "img@sha256:1".into())]),
            ..Default::default()
        };
        let p = platforms(&mapped);
        assert_eq!(p["ubuntu-22.04"], "img@sha256:1");
        assert!(p["ubuntu-24.04"].contains("27043bae70b3"));
        assert_eq!(p["ubuntu-latest"], p["ubuntu-24.04"]);

        // GitHub's other Linux labels, which act otherwise skips as unsupported
        let p = platforms(&config::Act::default());
        for (label, base) in [
            ("ubuntu-slim", "ubuntu-24.04"),
            ("ubuntu-24.04-arm", "ubuntu-24.04"),
            ("ubuntu-22.04-arm", "ubuntu-22.04"),
        ] {
            assert_eq!(p.get(label), Some(&p[base]), "{label}");
        }
    }

    #[test]
    fn trusts_the_mounted_repository_unless_the_job_configures_git() {
        let out = load(&inject(CI, "n").unwrap());
        assert_eq!(
            out["jobs"]["test"]["env"]["GIT_CONFIG_KEY_0"].as_str(),
            Some("safe.directory")
        );
        let own = CI.replace("    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4", "    runs-on: ubuntu-latest\n    env: {GIT_CONFIG_COUNT: '0'}\n    steps:\n      - uses: actions/checkout@v4");
        let out = load(&inject(&own, "n").unwrap());
        assert_eq!(
            out["jobs"]["test"]["env"]["GIT_CONFIG_COUNT"].as_str(),
            Some("0")
        );
        assert!(out["jobs"]["test"]["env"]["GIT_CONFIG_KEY_0"].is_badvalue());
    }

    #[test]
    fn finds_unsupported_platform_skips() {
        let log = "[ci/test__1] 🚧  Skipping unsupported platform -- Try running with `-P macos-latest=...`\n\
                   [ci/test__2] ⭐ Run Set up job\n";
        assert_eq!(unsupported_jobs(log), ["test__1"]);
    }

    fn combos(matrix: &str) -> Vec<String> {
        let m = load(matrix);
        legs(m.as_hash().unwrap())
            .unwrap()
            .iter()
            .map(|c| {
                c.iter()
                    .map(|(k, v)| format!("{}={}", k.as_str().unwrap(), v.as_str().unwrap()))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect()
    }

    #[test]
    fn matrix_axes_exclude_and_include_follow_github() {
        assert_eq!(
            combos("os: [a, b]\nv: ['1', '2']"),
            ["os=a,v=1", "os=a,v=2", "os=b,v=1", "os=b,v=2"]
        );
        assert_eq!(
            combos("os: [a, b]\nv: ['1', '2']\nexclude: [{os: b, v: '2'}]"),
            ["os=a,v=1", "os=a,v=2", "os=b,v=1"]
        );
        // extends the combinations it does not overwrite, else becomes a new one
        assert_eq!(
            combos("os: [a, b]\ninclude: [{os: a, x: y}, {os: c}, {extra: z}]"),
            ["os=a,x=y,extra=z", "os=b,extra=z", "os=c"]
        );
        assert_eq!(
            combos("include: [{os: a, fs: f1}, {os: b, fs: f2}]"),
            ["os=a,fs=f1", "os=b,fs=f2"]
        );
    }

    #[test]
    fn expression_matrices_are_left_to_act() {
        assert!(legs(load("os: ${{ fromJSON(inputs.list) }}").as_hash().unwrap()).is_none());
    }

    #[test]
    fn expands_matrix_jobs_and_rewrites_needs() {
        let wf = "\
on: push
jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        include:
          - {os: macos-latest, fs: apfs}
          - {os: ubuntu-latest, fs: ext4}
    runs-on: ${{ matrix.os }}
    steps:
      - run: echo ${{ matrix.fs }}
  release:
    needs: test
    runs-on: ubuntu-latest
    steps:
      - run: echo release
";
        let out = load(&inject(wf, "n").unwrap());
        let jobs = out["jobs"].as_hash().unwrap();
        let ids: Vec<&str> = jobs.keys().map(|k| k.as_str().unwrap()).collect();
        assert_eq!(ids, ["test__1", "test__2", "release"]);
        assert_eq!(
            out["jobs"]["test__1"]["runs-on"].as_str(),
            Some("macos-latest")
        );
        // act would give `ubuntu-latest` ImageOS ubuntu20
        assert_eq!(
            out["jobs"]["test__2"]["runs-on"].as_str(),
            Some("ubuntu-24.04")
        );
        assert_eq!(
            out["jobs"]["test__2"]["strategy"]["matrix"]["include"][0]["fs"].as_str(),
            Some("ext4")
        );
        assert_eq!(
            out["jobs"]["test__2"]["strategy"]["fail-fast"].as_bool(),
            Some(false)
        );
        assert_eq!(out["jobs"]["release"]["needs"][1].as_str(), Some("test__2"));
        assert_eq!(out["jobs"]["test__1"]["steps"].as_vec().unwrap().len(), 4);
    }

    #[test]
    fn resolves_a_matrix_runs_on_with_a_fallback() {
        // pallets/flask's tests.yaml
        let wf = "\
on: push
jobs:
  tests:
    runs-on: ${{ matrix.os || 'ubuntu-latest' }}
    strategy:
      matrix:
        include:
          - {python: '3.14', os: windows-latest}
          - {python: '3.13'}
          - {python: '3.12', os: ''}
    steps: [{run: echo}]
";
        let out = load(&inject(wf, "n").unwrap());
        let runs_on = |j: &str| out["jobs"][j]["runs-on"].as_str().map(str::to_string);
        assert_eq!(runs_on("tests__1").as_deref(), Some("windows-latest"));
        assert_eq!(runs_on("tests__2").as_deref(), Some("ubuntu-24.04"));
        assert_eq!(runs_on("tests__3").as_deref(), Some("ubuntu-24.04"));

        let leg = load("{os: macos-latest}").as_hash().unwrap().clone();
        let resolve = |v: &str| resolve_matrix(&key(v), &leg);
        assert_eq!(resolve("${{ matrix.os }}"), Some(key("macos-latest")));
        assert_eq!(
            resolve("${{ matrix.arch || matrix.os }}"),
            Some(key("macos-latest"))
        );
        assert_eq!(resolve("${{ matrix.arch }}"), None);
        assert_eq!(resolve("${{ matrix.arch || inputs.os }}"), None);
        assert_eq!(resolve("${{ matrix.os && 'x' }}"), None);
    }

    #[test]
    fn a_job_no_platform_maps_gets_no_network() {
        let labels: BTreeSet<String> = ["ubuntu-24.04".to_string()].into();
        let wf = "\
on: push
jobs:
  mac: {runs-on: macos-latest, steps: [{run: echo}]}
  list: {runs-on: [self-hosted, macos], steps: [{run: echo}]}
  linux: {runs-on: ubuntu-24.04, steps: [{run: echo}]}
  either: {runs-on: [macos-latest, ubuntu-24.04], steps: [{run: echo}]}
  expr: {runs-on: '${{ inputs.os }}', steps: [{run: echo}]}
  group: {runs-on: {group: big}, steps: [{run: echo}]}
";
        assert!(!runs_here(wf, "mac", &labels));
        assert!(!runs_here(wf, "list", &labels));
        assert!(runs_here(wf, "linux", &labels));
        assert!(runs_here(wf, "either", &labels));
        assert!(runs_here(wf, "expr", &labels));
        assert!(runs_here(wf, "group", &labels));
    }

    #[test]
    fn a_network_waits_for_an_address_pool_until_its_deadline() {
        let full =
            "Error response from daemon: all predefined address pools have been fully subnetted";
        let pause = Duration::from_millis(1);
        let later = Instant::now() + Duration::from_secs(60);
        let mut calls = 0;
        let created = until_pooled(later, pause, || {
            calls += 1;
            if calls < 3 { Err(full.into()) } else { Ok(()) }
        });
        assert_eq!((created, calls), (Ok(()), 3));

        let mut calls = 0;
        let other = until_pooled(later, pause, || {
            calls += 1;
            Err("permission denied".into())
        });
        assert_eq!((other, calls), (Err("permission denied".into()), 1));

        let past = Instant::now();
        let mut calls = 0;
        let late = until_pooled(past, pause, || {
            calls += 1;
            Err(full.into())
        });
        assert_eq!((late, calls), (Err(full.into()), 1));
    }

    #[test]
    fn a_job_that_failed_before_its_first_step_never_started() {
        let pulled = "[CI/a] ⭐ Run Set up job\n[CI/a]   🐳  docker pull image=catthehacker/ubuntu\n\
                      Error: context deadline exceeded\n";
        assert!(never_started(pulled));
        let ran = "[CI/a] ⭐ Run Main echo \"shipcheck-start n ${{ github.job }}\"\n\
                   [CI/a]   | shipcheck-start n a\n[CI/a]   | sh: cargo: not found\n";
        assert!(!never_started(ran));
    }

    #[test]
    fn resolves_continue_on_error_per_leg() {
        let wf = "\
on: push
jobs:
  t:
    runs-on: ubuntu-latest
    continue-on-error: ${{ matrix.experimental }}
    strategy:
      matrix:
        php: ['8.3']
        experimental: [false]
        include: [{php: nightly, experimental: true}]
    steps: [{run: echo}]
";
        let out = inject(wf, "n").unwrap();
        let parsed = load(&out);
        assert_eq!(
            parsed["jobs"]["t__1"]["continue-on-error"].as_bool(),
            Some(false)
        );
        assert_eq!(
            parsed["jobs"]["t__2"]["continue-on-error"].as_bool(),
            Some(true)
        );
        let allowed = workflow::allowed_failures(&out).unwrap();
        assert_eq!(allowed.into_iter().collect::<Vec<_>>(), ["t__2"]);
    }

    #[test]
    fn gives_listed_jobs_their_own_label() {
        let wf = "\
on: push
jobs:
  checks:
    runs-on: blacksmith-4vcpu-ubuntu-2404
    steps:
      - run: echo checks
  build:
    runs-on: blacksmith-4vcpu-ubuntu-2404
    strategy:
      matrix:
        part: [a, b]
    steps:
      - run: echo build
";
        let act = config::Act {
            jobs: BTreeMap::from([("ci.yml/build".to_string(), "img@sha256:amd64".to_string())]),
            ..Default::default()
        };
        let out = load(&inject_with(wf, "n", act).unwrap());
        assert_eq!(
            out["jobs"]["checks"]["runs-on"].as_str(),
            Some("blacksmith-4vcpu-ubuntu-2404")
        );
        assert_eq!(
            out["jobs"]["build__1"]["runs-on"].as_str(),
            Some("shipcheck-build")
        );
        assert_eq!(
            out["jobs"]["build__2"]["runs-on"].as_str(),
            Some("shipcheck-build")
        );
    }

    #[test]
    fn resolves_anchors_and_flow_style() {
        let wf = "\
on: [push]
x-steps: &steps
  - run: echo shared
jobs: {a: {runs-on: ubuntu-latest, steps: *steps}, b: {runs-on: ubuntu-latest, steps: *steps}}
";
        let out = load(&inject(wf, "n").unwrap());
        assert!(
            out["on"]
                .as_vec()
                .unwrap()
                .contains(&key("workflow_dispatch"))
        );

        for job in ["a", "b"] {
            let steps = out["jobs"][job]["steps"].as_vec().unwrap();
            assert_eq!(steps.len(), 4, "{job}");
            assert_eq!(steps[2]["run"].as_str(), Some("echo shared"));
        }
    }

    #[test]
    fn keeps_dispatch_once_and_its_inputs() {
        let wf = "\
on:
  workflow_dispatch:
    inputs:
      level: {default: low}
jobs: {}
";
        let out = load(&inject(wf, "n").unwrap());
        assert_eq!(
            out["on"]["workflow_dispatch"]["inputs"]["level"]["default"].as_str(),
            Some("low")
        );
    }

    #[test]
    fn marks_empty_steps_and_leaves_array_runs_on() {
        let wf = "\
on: push
jobs:
  empty:
    runs-on: [self-hosted, linux]
    steps: []
";
        let out = load(&inject(wf, "n").unwrap());
        assert_eq!(out["jobs"]["empty"]["steps"].as_vec().unwrap().len(), 3);
        assert_eq!(
            out["jobs"]["empty"]["runs-on"][0].as_str(),
            Some("self-hosted")
        );
    }

    #[test]
    fn rewrites_needs_through_chains_of_matrices() {
        let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    strategy: {matrix: {node: [18, 20]}}
    steps: [{run: echo a}]
  b:
    runs-on: ubuntu-latest
    needs: a
    steps: [{run: echo b}]
  c:
    runs-on: ubuntu-latest
    needs: [b, a]
    steps: [{run: echo c}]
";
        let out = load(&inject(wf, "n").unwrap());
        let needs = |job: &str| -> Vec<String> {
            out["jobs"][job]["needs"]
                .as_vec()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(needs("b"), ["a__1", "a__2"]);
        assert_eq!(needs("c"), ["b", "a__1", "a__2"]);
        assert_eq!(
            out["jobs"]["a__2"]["strategy"]["matrix"]["include"][0]["node"].as_i64(),
            Some(20)
        );
    }

    #[test]
    fn matrix_of_numbers_and_booleans() {
        assert_eq!(
            legs(
                load("node: [18, 20]\nexperimental: [true, false]")
                    .as_hash()
                    .unwrap()
            )
            .unwrap()
            .len(),
            4
        );
    }

    #[test]
    fn exclude_that_removes_everything_leaves_the_job_to_act() {
        let wf = "\
on: push
jobs:
  t:
    runs-on: ubuntu-latest
    strategy: {matrix: {os: [a], exclude: [{os: a}]}}
    steps: [{run: echo}]
";
        let out = load(&inject(wf, "n").unwrap());
        assert!(out["jobs"]["t"]["steps"].as_vec().is_some());
        assert!(out["jobs"]["t__1"].is_badvalue());
    }

    #[test]
    fn refuses_reusable_workflow_calls() {
        let act = Act::new(config::Act::default());

        for uses in [
            "./.github/workflows/other.yml",
            "acme/ci/.github/workflows/build.yml@v1",
        ] {
            let wf = format!("on: push\njobs:\n  call:\n    uses: {uses}\n");
            let err = act.prepare(&wf, "ci.yml").unwrap_err();
            assert!(err.contains("reusable workflow"), "{err}");
        }
    }

    #[test]
    fn the_artifact_server_listens_on_the_run_network_gateway_or_loopback() {
        assert_eq!(gateway("172.18.0.1 "), Some([172, 18, 0, 1].into()));
        assert_eq!(gateway("fd00::1 10.1.0.1"), Some([10, 1, 0, 1].into()));
        assert_eq!(gateway("8.8.8.8"), None);
        assert_eq!(gateway(""), None);
        assert_eq!(artifact_addr(None), "127.0.0.1");
        let net = Network {
            name: network_name(Path::new("/x")),
            gateway: [172, 18, 0, 1].into(),
        };
        let want = if cfg!(target_os = "macos") {
            "127.0.0.1"
        } else {
            "172.18.0.1"
        };
        assert_eq!(artifact_addr(Some(&net)), want);
        assert!(net.name.starts_with(&format!("{}net-", home_prefix())));
    }

    #[test]
    fn refuses_docker_options_that_reach_the_host() {
        let with = |job: &str| {
            format!("on: push\njobs:\n  t:\n    runs-on: x\n{job}    steps: [{{run: echo}}]\n")
        };
        let internet = Act::new(config::Act::default()).with_network(config::Network::Internet);

        for options in [
            "-v /:/host",
            "--volume=/x:/y",
            "-v/etc:/e",
            "--network host",
            "--privileged",
            "--cap-add SYS_ADMIN",
            "--sysctl net.ipv4.ip_forward=1",
            "--gpus all",
            "--cpus 2 --runtime runc",
        ] {
            let wf = with(&format!(
                "    container: {{image: node, options: '{options}'}}\n"
            ));
            let err = internet.prepare(&wf, "ci.yml").unwrap_err();
            assert!(err.contains("could reach the host"), "{options}: {err}");
        }

        let service = with("    services: {db: {image: postgres, options: '--network host'}}\n");
        assert!(
            internet
                .prepare(&service, "ci.yml")
                .unwrap_err()
                .contains("service options")
        );
        let quiet = with(
            "    services: {db: {image: postgres, options: '--health-cmd \"pg_isready -U postgres\" --health-interval 10s'}}\n",
        );
        assert!(internet.prepare(&quiet, "ci.yml").is_ok());
        let offline = Act::new(config::Act::default());
        assert!(
            offline
                .prepare(&quiet, "ci.yml")
                .unwrap_err()
                .contains("need a network")
        );
        let bind = with("    container: {image: node, volumes: ['/:/host']}\n");
        let err = internet.prepare(&bind, "ci.yml").unwrap_err();
        assert!(err.contains("sets volumes"), "{err}");
        for job in [
            "    container: {image: node, options: '${{ matrix.o }}'}\n",
            "    services: {db: {image: postgres, options: '${{ matrix.o }}'}}\n",
            "    services: ${{ fromJSON(inputs.s) }}\n",
            // The image exemption holds only at a container's own top level
            "    services: {db: {image: p, ports: [{image: '${{ matrix.o }}'}]}}\n",
            "    container: ${{ fromJSON(inputs.c) }}\n",
            "    container: {image: node, options: '--cpus 2 \\\\ --privileged'}\n",
            "    container: {image: node, options: '--cpus \"2'}\n",
            "    container: {image: node, options: ['--privileged']}\n",
        ] {
            assert!(internet.prepare(&with(job), "ci.yml").is_err(), "{job}");
        }

        for job in [
            "    strategy: {matrix: {image: [node, 'python:3']}}\n    container: ${{ matrix.image }}\n",
            "    container: {image: '${{ matrix.image }}', env: {A: '${{ matrix.a }}'}}\n",
        ] {
            assert!(internet.prepare(&with(job), "ci.yml").is_ok(), "{job}");
        }

        // A matrix value can be a whole container mapping with its own options
        let smuggled = "    strategy: {matrix: {c: [{image: node, options: '--privileged'}]}}\n    container: ${{ matrix.c }}\n";
        let err = internet.prepare(&with(smuggled), "ci.yml").unwrap_err();
        assert!(err.contains("--privileged"), "{err}");
        let unresolved = "    container: ${{ matrix.image }}\n";
        assert!(internet.prepare(&with(unresolved), "ci.yml").is_err());

        let service_bind = with("    services: {db: {image: postgres, volumes: ['/etc:/e']}}\n");
        let err = internet.prepare(&service_bind, "ci.yml").unwrap_err();
        assert!(err.contains("sets volumes"), "{err}");
        assert!(
            offline
                .prepare(
                    &with("    container: {image: node, options: '--cpus 2'}\n"),
                    "ci.yml"
                )
                .is_ok()
        );
    }

    #[test]
    fn allows_only_the_listed_docker_options_in_every_form() {
        let internet = Act::new(config::Act::default()).with_network(config::Network::Internet);
        let check = |options: &str| {
            // Debug quoting is also a valid YAML double-quoted string
            let wf = format!(
                "on: push\njobs:\n  t:\n    runs-on: x\n    container: {{image: node, options: {options:?}}}\n    services: {{db: {{image: pg, options: {options:?}}}}}\n    steps: [{{run: echo}}]\n"
            );
            internet.prepare(&wf, "ci.yml")
        };

        for refused in [
            "--privileged",
            "--privileged=true",
            "--cap-add ALL",
            "--cap-add=SYS_PTRACE",
            "--cap-drop ALL",
            "--pid host",
            "--pid=host",
            "--ipc host",
            "--uts host",
            "--userns host",
            "--cgroupns host",
            "--cgroup-parent x",
            "--network host",
            "--network=container:x",
            "--net host",
            "--add-host h:10.0.0.1",
            "--dns 1.1.1.1",
            "-p 80:80",
            "--publish-all",
            "--device /dev/kvm",
            "--device-cgroup-rule a",
            "--gpus all",
            "--security-opt seccomp=unconfined",
            "--security-opt apparmor=unconfined",
            "--mount type=bind,src=/,dst=/host",
            "-v /:/host",
            "--volume /:/host",
            "--volumes-from x",
            "--tmpfs /x",
            "--env-file /etc/passwd",
            "--label-file /etc/passwd",
            "--runtime runc",
            "--sysctl a=b",
            "--group-add 0",
            "--link x",
            "--log-driver none",
            "--name x",
            "-it",
            "-eA=1",
            "--cpus 2 -v/:/host",
            // Quotes hide no flag, since act's splitter strips them too
            "\"--privileged\"",
            "'--privileged'",
            "--cpus=2 '--priv'ileged",
            "--health-cmd \"--privileged\"",
            // Characters act's splitter reads differently, and expressions act would expand later
            "--cpus 2 \\ --privileged",
            "--cpus 2\n--privileged",
            "--cpus 2\t--privileged",
            "--cpus 2; --privileged",
            "--cpus $(x)",
            "--cpus ${{ matrix.c }}",
            "--cpus `x`",
            "--cpus \"2",
        ] {
            let err = check(refused).expect_err(refused);
            assert!(err.contains("options"), "{refused:?}: {err}");
        }

        for allowed in [
            "--cpus 2",
            "--memory=4g -m 4g --memory-swap 8g --shm-size 2g",
            "--env A=1 -e B=2",
            "--env \"A=a b --privileged\"",
            "--health-cmd \"pg_isready -U postgres\" --health-interval 10s --health-timeout 5s --health-retries 5 --health-start-period 1s",
            "--no-healthcheck",
            "--user 1000:1000 -u 0 --workdir /w -w /w --entrypoint sh",
            "--hostname h -h h --ulimit nofile=1024:2048 --init",
        ] {
            assert!(check(allowed).is_ok(), "{allowed:?}: {:?}", check(allowed));
        }

        // A matrix leg that becomes a whole container is checked as the value it becomes
        let wf = "on: push\njobs:\n  t:\n    runs-on: x\n    strategy: {matrix: {c: [{image: a, options: '--cpus 1'}, {image: b, volumes: ['/:/h']}]}}\n    container: ${{ matrix.c }}\n    steps: [{run: echo}]\n";
        let err = internet.prepare(wf, "ci.yml").unwrap_err();
        assert!(err.contains("sets volumes"), "{err}");
    }

    #[test]
    fn evaluates_simple_continue_on_error_conditions_per_leg() {
        let leg = load("ruby: 3.4-head\nos: ubuntu\nn: 3")
            .as_hash()
            .unwrap()
            .clone();
        let cases = [
            ("${{ endsWith(matrix.ruby, 'head') }}", Some(true)),
            ("${{ startsWith(matrix.ruby, '3.3') }}", Some(false)),
            ("${{ contains(matrix.ruby, '4-h') }}", Some(true)),
            ("${{ matrix.os == 'ubuntu' }}", Some(true)),
            ("${{ matrix.os != 'ubuntu' }}", Some(false)),
            ("${{ matrix.n == '3' }}", Some(true)),
            ("${{ true }}", Some(true)),
            // Anything else stays unknown, so the failure counts
            ("${{ matrix.missing == 'x' }}", None),
            ("${{ github.ref == 'refs/heads/main' }}", None),
            ("${{ fromJSON(inputs.allow) }}", None),
            ("plain", None),
        ];

        for (expr, want) in cases {
            assert_eq!(matrix_condition(expr, &leg), want, "{expr}");
        }

        let wf = "\
on: push
jobs:
  t:
    runs-on: ubuntu-latest
    continue-on-error: ${{ endsWith(matrix.ruby, 'head') }}
    strategy: {matrix: {ruby: ['3.4', head]}}
    steps: [{run: echo}]
";
        let out = inject(wf, "n").unwrap();
        let allowed = workflow::allowed_failures(&out).unwrap();
        assert_eq!(allowed.into_iter().collect::<Vec<_>>(), ["t__2"]);
    }

    #[test]
    fn refuses_engines_with_known_escapes() {
        let ok = "Docker Desktop 4.90.0 (238679)|Engine=29.7.2;containerd=v2.3.3;runc=1.4.3;";
        assert_eq!(engine_problem(ok), None);
        assert!(
            engine_problem("Docker Desktop 4.44.2 (1)|runc=1.4.3;")
                .unwrap()
                .contains("CVE-2025-9074")
        );
        assert_eq!(
            engine_problem("Docker Desktop 4.44.3 (1)|runc=1.3.3;"),
            None
        );

        for runc in ["1.1.12", "1.2.7", "1.3.2", "v1.0.0", "1.4.0-rc.2"] {
            let report = format!("Docker Engine - Community|runc={runc};");
            assert!(engine_problem(&report).is_some(), "{runc}");
        }

        for runc in ["1.2.8", "1.3.3", "1.4.0-rc.3", "1.4.0", "v1.5.1"] {
            let report = format!("Docker Engine - Community|runc={runc};");
            assert_eq!(engine_problem(&report), None, "{runc}");
        }

        // An engine without runc, such as one on crun, is not a known problem
        assert_eq!(engine_problem("Podman Engine|Podman Engine=5.0.0;"), None);
    }

    #[test]
    fn rejects_non_workflows() {
        assert!(inject("- a\n- b\n", "n").is_err());
        assert!(inject("jobs: {}\n", "n").is_err());
        assert!(inject("on: push\n", "n").is_err());
    }

    #[test]
    fn prints_job_outputs_in_a_last_step() {
        let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    outputs:
      version: ${{ steps.v.outputs.version }}
      bad name: x
    steps:
      - id: v
        run: echo version=1 >> $GITHUB_OUTPUT
";
        let out = load(&inject(wf, "n").unwrap());
        let steps = out["jobs"]["a"]["steps"].as_vec().unwrap();
        let printed = &steps[steps.len() - 2];
        assert_eq!(
            printed["env"]["SHIPCHECK_OUTPUT_0"].as_str(),
            Some("${{ steps.v.outputs.version }}")
        );
        assert!(printed["env"]["SHIPCHECK_OUTPUT_1"].is_badvalue());
        assert!(printed["run"].as_str().unwrap().contains("'version'"));
    }

    #[test]
    fn reads_each_steps_summary_from_the_log() {
        // As act 0.2.89 logs two steps that wrote a summary, with step output between them
        let log = "[ci.yml/t] \u{2b50} Run Main printf\n\
                   [ci.yml/t]   \u{2699}  Summary - | a | b |\n\
                   |---|---|\n\
                   | 1 | 2 |\n\
                   [ci.yml/t] unable to get git ref\n\
                   [ci.yml/t]   | [x/y]   \u{2699}  Summary - forged\n\
                   [ci.yml/t]   \u{2699}  Summary - second\n\
                   [ci.yml/t] \u{2b50} Run Complete job\n";
        assert_eq!(read_summary(log), "| a | b |\n|---|---|\n| 1 | 2 |\nsecond");
        assert_eq!(read_summary("[ci.yml/t] nothing\n"), "");
        let long = format!("[w/j]   \u{2699}  Summary - {}\n", "\u{e9}".repeat(20_000));
        assert!(read_summary(&long).ends_with("[...cut at 20000 bytes...]"));
    }

    #[test]
    fn reads_outputs_from_the_log() {
        // Lines as the outputs step prints them, plus act echoing the command first
        let log = "[CI/a] | printf 'shipcheck-output %s %s\\n' 'v' \"$(...)\"\n\
                   [CI/a]   | shipcheck-output v 6120620a27712720247820c3a9\n\
                   [CI/a]   | shipcheck-output e \n\
                   [CI/a]   | shipcheck-output odd 612\n";
        let out = read_outputs(log);
        assert_eq!(out["v"], "a b\n'q' $x é");
        assert_eq!(out["e"], "");
        assert!(!out.contains_key("odd"));
        assert_eq!(out.len(), 2);
    }

    fn outputs(entries: &[(&str, &[(&str, &str)])]) -> BTreeMap<String, BTreeMap<String, String>> {
        entries
            .iter()
            .map(|(job, kv)| {
                let kv = kv.iter().map(|(k, v)| (k.to_string(), v.to_string()));
                (job.to_string(), kv.collect())
            })
            .collect()
    }

    #[test]
    fn isolates_one_job_with_its_needs_as_literals() {
        let wf = "\
name: CI
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    steps: [{run: echo test}]
  build:
    runs-on: ubuntu-latest
    steps: [{run: echo build}]
  deploy:
    needs: [test__1, build]
    if: needs.build.result == 'success'
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ needs.build.outputs.tag }} ${{ needs.test.outputs.os }} ${{ needs.build.outputs.none }}
";
        let needs = outputs(&[("test__1", &[("os", "it's")]), ("build", &[("tag", "v1")])]);
        let out = load(&isolate(wf, "ci.yml", "deploy", &needs, "abcd", "5eed").unwrap());
        assert_eq!(out["name"].as_str(), Some("CI"), "github.workflow stays");
        assert_eq!(out["jobs"]["deploy"]["name"].as_str(), Some("deploy abcd"));
        let jobs = out["jobs"].as_hash().unwrap();
        assert_eq!(jobs.len(), 1);
        let job = &out["jobs"]["deploy"];
        assert!(job["needs"].is_badvalue());
        assert_eq!(job["if"].as_str(), Some("'success' == 'success'"));
        let guard = job["steps"][0]["run"].as_str().unwrap();
        assert!(guard.contains("/.git/shipcheck-nonce") && guard.contains("!= 5eed ]"));
        assert_eq!(job["steps"][0]["shell"].as_str(), Some("sh"));
        assert_eq!(
            job["steps"][1]["run"].as_str(),
            Some(
                "echo ${{ fromJSON('\"v1\"') }} ${{ fromJSON('\"it\\u0027s\"') }} ${{ fromJSON('\"\"') }}"
            )
        );
    }

    #[test]
    fn outputs_reach_the_next_job_as_literal_strings() {
        let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps: [{run: echo a}]
  b:
    needs: a
    if: needs.a.outputs.v != ''
    runs-on: ubuntu-latest
    env:
      E: ${{ needs.a.outputs.v }}
    steps:
      - run: echo needs.a.outputs.v
      - run: echo \"${{ needs.a.outputs.v }}\"
";
        let value = "x' }} ${{ secrets.S }} ${{ '\n- run: evil\nkey: [a, {b: \"c\n#";
        let needs = outputs(&[("a", &[("v", value)])]);
        let out = load(&isolate(wf, "ci.yml", "b", &needs, "t", "n").unwrap());
        let literal = literal(value);
        // Nothing of the value can end the expression or start one, wherever act scans for them
        let inner = &literal["fromJSON('".len()..literal.len() - "')".len()];
        assert!(!inner.contains(['\'', '{', '}', '$', '\n']), "{literal}");
        assert_eq!(serde_json::from_str::<String>(inner).unwrap(), value);
        let job = &out["jobs"]["b"];
        assert_eq!(out["jobs"].as_hash().unwrap().len(), 1);
        assert_eq!(
            job["steps"].as_vec().unwrap().len(),
            3,
            "the checkout step and the two steps"
        );
        assert_eq!(job["if"].as_str().unwrap(), format!("{literal} != ''"));
        // act evaluates a job's env twice, so the value moves to each step's env
        assert!(job["env"]["E"].is_badvalue());

        for step in &job["steps"].as_vec().unwrap()[1..] {
            assert_eq!(
                step["env"]["E"].as_str().unwrap(),
                format!("${{{{ {literal} }}}}")
            );
        }
        assert_eq!(
            job["steps"][2]["run"].as_str().unwrap(),
            format!("echo \"${{{{ {literal} }}}}\"")
        );
        // Outside an expression GitHub reads it as plain text, and so must shipcheck, or the value
        // would land where act expands `${{ }}`
        assert_eq!(
            job["steps"][1]["run"].as_str(),
            Some("echo needs.a.outputs.v")
        );
    }

    #[test]
    fn refuses_needs_in_an_env_it_cannot_move() {
        let needs = outputs(&[("a", &[("v", "${{ secrets.S }}")])]);
        let with = |job_env: &str, step: &str| {
            format!(
                "on: push\njobs:\n  a: {{runs-on: x, steps: [{{run: echo}}]}}\n  b:\n    needs: a\n    runs-on: x\n{job_env}    steps:\n      - {step}\n"
            )
        };

        for wf in [
            with("    env: ${{ needs.a.outputs.v }}\n", "run: echo"),
            with(
                "    env: {E: '${{ needs.a.outputs.v }}'}\n",
                "{run: echo, env: '${{ fromJSON(x) }}'}",
            ),
            with(
                "    env: {E: '${{ needs.a.outputs.v }}'}\n",
                "{run: echo, env: [x]}",
            ),
        ] {
            let err = isolate(&wf, "ci.yml", "b", &needs, "t", "n").unwrap_err();
            assert!(err.contains("not a mapping"), "{wf}: {err}");
        }

        // An env with nothing from needs is left as it is
        let plain = with("    env: ${{ fromJSON(x) }}\n", "{run: echo, env: [x]}");
        assert!(isolate(&plain, "ci.yml", "b", &needs, "t", "n").is_ok());
        let empty = with(
            "    env: {E: '${{ needs.a.outputs.v }}'}\n",
            "{run: echo, env: }",
        );
        let out = load(&isolate(&empty, "ci.yml", "b", &needs, "t", "n").unwrap());
        assert!(out["jobs"]["b"]["steps"][1]["env"]["E"].as_str().is_some());
    }

    #[test]
    fn refuses_needs_it_cannot_pass() {
        let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps: [{run: echo a}]
  b:
    needs: a
    runs-on: ubuntu-latest
    steps:
      - run: echo '${{ toJSON(needs) }}'
";
        let needs = outputs(&[("a", &[])]);
        let err = isolate(wf, "ci.yml", "b", &needs, "t", "n").unwrap_err();
        assert!(err.contains("toJSON(needs)"), "{err}");
        // Words that only contain `needs`, and `needs` in a script, are fine
        let ok = wf.replace(
            "toJSON(needs)",
            "steps.needs_x.outputs.y }} needs.a.result ${{ inputs.needs",
        );
        assert!(isolate(&ok, "ci.yml", "b", &needs, "t", "n").is_ok());
        assert!(isolate(wf, "ci.yml", "missing", &needs, "t", "n").is_err());
    }

    #[test]
    fn volume_names_are_docker_safe() {
        let name = volume_name("Acme/App.rs", "/root/.rustup");
        assert!(name.starts_with(&home_prefix()) && name.contains("-acme-app-rs-"));
        assert!(name.ends_with("-root--rustup"));
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
    }

    #[test]
    fn the_prune_never_lists_a_current_pin() {
        for (_, image) in DEFAULT_PLATFORMS {
            assert!(!gc::OLD_PINS.contains(&image));
        }

        assert!(!gc::OLD_PINS.contains(&BUSYBOX));
    }

    #[test]
    fn slot_paths_get_a_volume_per_slot_and_job() {
        let act = config::Act::default();
        let run = Path::new("/r");
        let a = mounts(&act, "o/r", "aaaa", run).unwrap();
        let b = mounts(&act, "o/r", "bbbb", run).unwrap();
        let apt = |m: &[String]| {
            m.iter()
                .find(|m| m.ends_with(":/var/lib/apt/lists"))
                .cloned()
        };
        assert_ne!(apt(&a), apt(&b));
        assert!(apt(&a).unwrap().contains("-aaaa-var-lib-apt-lists:"));
        let npm = |m: &[String]| {
            m.iter()
                .find(|m| m.ends_with(":/home/runner/.npm"))
                .cloned()
        };
        assert_eq!(npm(&a), npm(&b));
        assert_eq!(a.last().unwrap(), "--mount 'type=volume,dst=/r/.venv'");
    }

    #[test]
    fn volume_prefixes_never_overlap_between_repos() {
        let (ab, abc) = (volume_prefix("a/b"), volume_prefix("a/b-c"));
        assert!(!volume_name("a/b-c", "/x").starts_with(&ab));
        assert_eq!(
            venv_mount(Path::new("/Users/a/Application Support/r")).unwrap(),
            "--mount 'type=volume,dst=/Users/a/Application Support/r/.venv'"
        );
        assert!(venv_mount(Path::new("/a'b")).is_err());
        assert!(!volume_name("a/b", "/x").starts_with(&abc));
        assert!(volume_name("a/b", "/x").starts_with(&ab));
    }

    #[test]
    fn goes_offline_only_when_every_cached_action_resolves_its_ref() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cached_actions_resolve(&dir.path().join("none")));

        let action = dir.path().join("o-a@v1");
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&action)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        fs::create_dir_all(&action).unwrap();
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "a"]);
        git(&["tag", "v1"]);
        fs::create_dir(dir.path().join("workspace")).unwrap();
        assert!(cached_actions_resolve(dir.path()));

        fs::create_dir(dir.path().join("o-b@v2")).unwrap();
        assert!(
            !cached_actions_resolve(dir.path()),
            "a clone that lost its ref"
        );
    }
}
