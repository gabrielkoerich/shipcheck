use super::{
    BUSYBOX, Checkout, Engine, JobSettings, RUNNER_ID, actions_dir, cache_dir,
    cached_actions_resolve, engine_problem, engine_resources, isolated, job_actions, job_label,
    platforms, read_outputs, read_summary, run_tag, runs_here, skipped_by_if, unsupported_jobs,
};
use super::{Outcome, RunRequest, gc, shard};
use crate::workflow::{self, key};
use crate::{cache, clock, config, paths, runner, secret};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use yaml_rust2::Yaml;
use yaml_rust2::yaml::Hash;

/// Each job in a container on the current Docker context
#[derive(Debug)]
pub struct Docker;

impl Engine for Docker {
    fn name(&self) -> &'static str {
        "docker"
    }

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

        engine_problem(String::from_utf8_lossy(&out.stdout).trim()).map_or(Ok(()), Err)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one act process, from its arguments to its outcome"
    )]
    fn run(
        &self,
        settings: &JobSettings<'_>,
        request: &RunRequest<'_>,
        log_path: &Path,
    ) -> Result<Outcome, String> {
        let run_dir = request.run_dir;
        let began = Instant::now();
        let (file, job) = (request.file, request.job);
        let wf_path = request.workflow;
        let tag = run_tag(run_dir);
        let isolated = isolated(request, &tag, Checkout::Shared)?;
        let shards = (request.shard && isolated.contains(shard::MARK))
            .then(|| shard::count(settings.shards(), engine_resources()));

        if let Some(n) = shards {
            shard::write_plugin(run_dir, n)?;
        }

        fs::write(wf_path, keep_caches(run_dir, &isolated)?).map_err(|e| e.to_string())?;

        let log = fs::File::create(log_path).map_err(|e| e.to_string())?;
        let labels: BTreeSet<String> = settings.labels().iter().cloned().collect();
        // act skips a job with no image without starting a container, and Docker has only about
        // 30 address pools for bridges, which a matrix of macOS legs would take for nothing
        let network = match settings.network() {
            config::Network::Internet if runs_here(&isolated, request.job, &labels) => {
                Some(create_network(run_dir, request.timeout)?)
            }
            _ => None,
        };
        let mut cmd = Command::new("act");
        let port = free_port()?;
        let home = paths::home();

        // Stamped first, so a parallel run's prune never takes it for an empty one no run used
        for (name, path) in volumes(settings.config(), request.repo_id, &tag) {
            gc::stamp(&home, "volumes", &name);
            gc::create_volume(&name, &home, request.repo_id, path)?;
        }

        let volumes = mounts(settings.config(), request.repo_id, &tag, run_dir)?;

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

        for (label, image) in platforms(settings.config()) {
            cmd.arg("-P").arg(format!("{label}={image}"));
        }

        for (job, image) in settings.config().job_images(request.file) {
            cmd.arg("-P").arg(format!("{}={image}", job_label(&job)));
        }

        // On a Mac the secrets live in the keychain, so act reads a private copy that lives as long as it
        let secrets = secret::read(&paths::secrets_file(request.repo_id))?;
        let secrets_copy = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        fs::write(secrets_copy.path(), secrets.unwrap_or_default()).map_err(|e| e.to_string())?;
        cmd.arg("--secret-file").arg(secrets_copy.path());
        let actions = job_actions(request)?;

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

        clock::timing(file, job, "act prepare", began);
        let spawned = Instant::now();
        let mut reaper = reaper(run_dir, request.slot_lock);
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
                        clock::timing(file, job, phase, spawned);
                    }

                    !seen
                });
            }

            // The reaper hears before the child is reaped, while no other process can have its id
            let ended = runner::has_exited(child.id());

            if matches!(ended, Ok(true)) {
                clock::timing(file, job, "act total", spawned);

                // act ended by itself and removed its containers, so the reaper has nothing to do
                if let Some(stdin) = reaper.as_mut().and_then(|r| r.stdin.as_mut()) {
                    let _ = writeln!(stdin, "done");
                }

                break child.wait().map_err(|e| e.to_string())?.code();
            }

            // A child shipcheck cannot see end is stopped as on a timeout, never taken as done
            if ended.is_err() || Instant::now() > deadline {
                // The group, act's docker commands too, while its id is still act's
                runner::signal_group(child.id(), libc::SIGKILL);

                while !runner::has_exited(child.id()).unwrap_or(true) {
                    std::thread::sleep(Duration::from_millis(20));
                }

                if let Some(stdin) = reaper.as_mut().and_then(|r| r.stdin.as_mut()) {
                    let _ = writeln!(stdin, "exited");
                }

                let _ = child.wait();
                break None;
            }

            std::thread::sleep(Duration::from_millis(if timed { 50 } else { 200 }));
        };

        // Without `done`, as after a timeout, it removes what act left
        if let Some(mut reaper) = reaper {
            drop(reaper.stdin.take());
            let _ = reaper.wait();
        }

        let at = Instant::now();
        hand_back(run_dir);
        clock::timing(file, job, "hand_back", at);
        let log = fs::read(log_path).map_err(|e| e.to_string())?;
        let log = String::from_utf8_lossy(&log);

        for image in gc::images_in_log(&log) {
            gc::stamp(&home, "images", &image);
        }

        // A job its `if:` skips prints nothing, so a dry run, which starts no container, says why
        let skipped = exit == Some(0) && !log.contains("shipcheck-start ") && skipped_by_if(&cmd);

        Ok(Outcome {
            exit,
            skipped,
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
        let (home, prefix, host) = here_parts();
        gc::prune(
            &gc::Docker,
            &here(&home, &prefix, &host),
            &run_tags(&home),
            gc::OLD_PINS,
            std::time::SystemTime::now(),
        )
    }

    fn clean(&self, repo_id: Option<&str>) -> Vec<String> {
        let mut done = Vec::new();

        for dir in [cache_dir(repo_id), actions_dir(repo_id)] {
            if dir.exists() && fs::remove_dir_all(&dir).is_ok() {
                done.push(format!("removed {}", dir.display()));
            }
        }

        let prefix = repo_id.map_or_else(home_prefix, volume_prefix);
        let (home, own, host) = here_parts();
        let here = here(&home, &own, &host);
        let orphans = if repo_id.is_none() {
            gc::Orphans::Remove
        } else {
            gc::Orphans::Keep
        };
        done.extend(gc::clean(&gc::Docker, &here, &prefix, orphans));
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

/// Gives a checkout back to the user after a job, see `chown_tree`
pub(super) fn hand_back(dir: &Path) {
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

pub(super) fn chown_tree(dir: &Path, owner: Option<String>) {
    let Some(owner) = owner.filter(|_| cfg!(target_os = "linux") && dir.exists()) else {
        return;
    };

    gc::stamp(&paths::home(), "images", BUSYBOX);
    let _ = Command::new("docker")
        .args(["run", "--rm", "--network", "none", "-v"])
        .arg(format!("{}:/w", dir.display()))
        .args([BUSYBOX, "chown", "-R", &owner, "/w"])
        .output();
}

/// The paths act's containers mount a run's directory at. Docker matches the exact path, never a
/// parent, and act mounts the one its working directory resolves to, /private/var for /var on macOS
pub(super) fn mount_paths(run_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![run_dir.to_path_buf()];

    if let (Some(parent), Some(name)) = (run_dir.parent(), run_dir.file_name())
        && let Ok(real) = parent.canonicalize()
    {
        paths.push(real.join(name));
    }

    paths
}

pub(super) const REAPER: &str = r#"read -r pgid || exit 0
read -r state || state=
[ "$state" = done ] && exit 0
[ "$state" = exited ] || kill -s KILL -- "-$pgid" 2>/dev/null
net=$1
tag=$2
shift 2
cleanup() {
  for p in "$@"; do
    docker ps -aq --filter name=^act- --filter "volume=$p" | while read -r id; do docker rm -f -v "$id"; done
  done
  # act removes its volumes only when it ends by itself
  docker volume ls -q --filter "name=-$tag-" | while read -r v; do
    case $v in act-*-"$tag"-*-env) docker volume rm "$v" ;; esac
  done
  docker network rm "$net"
}
# The slot's lock this reaper holds stays out of docker's processes, and a hung Docker gives up
closed=
for fd in $LOCK_FDS; do closed="$closed $fd>&-"; done
eval "cleanup \"\$@\" $closed" &
d=$!
n=0
limit=${CLEANUP_SECS:-120}
while kill -0 "$d" 2>/dev/null && [ "$n" -lt "$limit" ]; do sleep 1; n=$((n + 1)); done
kill -s KILL "$d" 2>/dev/null
wait "$d" 2>/dev/null
exit 0
"#;

pub(super) fn reaper(
    run_dir: &Path,
    slot_lock: Option<std::os::fd::RawFd>,
) -> Option<std::process::Child> {
    let mut cmd = Command::new("sh");
    let held = runner::inherit(&mut cmd, slot_lock.into_iter().collect());
    cmd.env(
        "LOCK_FDS",
        held.iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(" "),
    );
    cmd.args([
        "-c",
        REAPER,
        "sh",
        &network_name(run_dir),
        &run_tag(run_dir),
    ])
    .args(mount_paths(run_dir))
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .process_group(0)
    .spawn()
    .ok()
}

pub(super) fn network_name(run_dir: &Path) -> String {
    format!("{}net-{}", home_prefix(), run_tag(run_dir))
}

/// A Docker network of the run's own, which `reclaim` removes
pub(super) struct Network {
    pub(super) name: String,
    pub(super) gateway: std::net::Ipv4Addr,
}

pub(super) fn create_network(run_dir: &Path, wait: Duration) -> Result<Network, String> {
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
pub(super) const POOL_WAIT: Duration = Duration::from_mins(10);

/// Calls `create` until it succeeds, fails for another reason than a lack of address pools, or the
/// deadline passes. Other runs' jobs hold every pool, and each gives its pool back when it ends
pub(super) fn until_pooled(
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

pub(super) fn pools_taken(err: &str) -> bool {
    err.contains("all predefined address pools have been fully subnetted")
}

pub(super) fn gateway(inspect: &str) -> Option<std::net::Ipv4Addr> {
    inspect
        .split_whitespace()
        .find_map(|a| a.parse::<std::net::Ipv4Addr>().ok())
        .filter(std::net::Ipv4Addr::is_private)
}

/// A bridge container on Linux reaches the host only at its network's gateway, never the host's
/// loopback. act gives jobs this address too, unless `ACTIONS_RUNTIME_URL` is set as on a Mac
pub(super) fn artifact_addr(network: Option<&Network>) -> String {
    match network {
        Some(n) if !cfg!(target_os = "macos") => n.gateway.to_string(),
        _ => "127.0.0.1".into(),
    }
}

pub(super) fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("no free port for the artifact server: {e}"))
}

pub(super) fn docker_safe(s: &str) -> String {
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
pub(super) fn volume_prefix(repo_id: &str) -> String {
    let hash = cache::hex(&Sha256::digest(repo_id.as_bytes())[..4]);
    format!("{}{}-{hash}-", home_prefix(), docker_safe(repo_id))
}

/// The start of every volume name under this `SHIPCHECK_HOME`, so `clean --all` of one home leaves
/// the volumes of another, such as a test's, alone
pub fn home_prefix() -> String {
    prefix_of(&paths::home())
}

pub fn prefix_of(home: &Path) -> String {
    let hash = cache::hex(&Sha256::digest(home.to_string_lossy().as_bytes())[..3]);
    format!("shipcheck-{hash}-")
}

/// The home, its prefix and the host name, which `here` borrows
pub fn here_parts() -> (PathBuf, String, String) {
    let home = paths::home();
    let prefix = prefix_of(&home);
    (home, prefix, gc::hostname())
}

pub fn here<'a>(home: &'a Path, prefix: &'a str, host: &'a str) -> gc::Here<'a> {
    gc::Here {
        home,
        prefix,
        host,
        sweep: !paths::home_overridden(),
    }
}

/// The run tags of every checkout dir under `runs/<owner>/<repo>/<slot>`, and of the slot itself,
/// which was the checkout before one per job
pub fn run_tags(home: &Path) -> Vec<String> {
    let mut dirs = vec![home.join("runs")];
    let mut tags = Vec::new();

    for depth in 1..=5 {
        dirs = dirs
            .iter()
            .flat_map(|d| fs::read_dir(d).into_iter().flatten().flatten())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();

        if depth >= 3 {
            tags.extend(dirs.iter().map(|d| run_tag(d)));
        }
    }

    tags
}

pub(super) fn keep_caches(run_dir: &Path, workflow: &str) -> Result<String, String> {
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

pub(super) fn no_prune(steps: &mut Vec<Yaml>) -> Vec<String> {
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
                .map(|(run, cwd)| install_step(step, &run, cwd.as_deref()))
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
pub(super) fn pnpm_installs(input: &Yaml) -> Option<Vec<(String, Option<String>)>> {
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
                        .collect::<Option<_>>()?;
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
pub(super) fn install_step(setup: &Hash, run: &str, cwd: Option<&str>) -> Yaml {
    let mut h = Hash::new();
    h.insert(key("name"), key(run));

    for field in ["if", "env", "continue-on-error"] {
        if let Some(v) = setup.get(&key(field)) {
            h.insert(key(field), v.clone());
        }
    }

    h.insert(key("run"), key(run));
    // Composite actions need a shell on every run step
    h.insert(key("shell"), key("bash"));

    if let Some(dir) = cwd {
        h.insert(key("working-directory"), key(dir));
    }

    Yaml::Hash(h)
}

/// The `action.yml` of a `uses: ./dir` action inside the checkout, if no part of its path is a link
pub(super) fn local_action(run_dir: &Path, uses: &str) -> Option<PathBuf> {
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
pub(super) fn venv_mount(run_dir: &Path) -> Result<String, String> {
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
pub(super) fn volumes<'a>(
    config: &'a config::Act,
    repo_id: &str,
    tag: &str,
) -> Vec<(String, &'a String)> {
    let shared = config
        .cache_paths
        .iter()
        .map(|p| (volume_name(repo_id, p), p));
    let slot = (config.slot_paths.iter()).map(|p| (volume_name(repo_id, &format!("{tag}{p}")), p));
    shared.chain(slot).collect()
}

/// `cache_paths` share a volume per repository, `slot_paths` get one per slot and job, as `tag` names
pub(super) fn mounts(
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

pub(super) fn volume_name(repo_id: &str, path: &str) -> String {
    format!(
        "{}{}",
        volume_prefix(repo_id),
        docker_safe(path.trim_start_matches('/'))
    )
}
