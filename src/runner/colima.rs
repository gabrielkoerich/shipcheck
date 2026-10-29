use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::act::{self, Act, Engine};
use super::{Outcome, RunRequest};
use crate::cache;

/**
 * Each job on a Colima VM of its own, started for the job and deleted after it. The VM has no shares
 * and no port forwards, the tree and a `.git` of its own go in over `colima ssh`, and act runs inside
 * with the job's labels on `-self-hosted`, so the job's steps run on the VM with its own Docker and
 * kernel. The VM reaches the Mac's loopback through its gateway, so it is for trusted repositories
 **/
pub struct Colima;

/// The VM's packages, act pinned as ci.yml pins it, Node 24 LTS as GitHub runs actions on, and
/// GitHub's `/home/runner`. Part of the key. Ubuntu's own nodejs is 18, which fails setup-uv
const SETUP: &str = r#"set -eu
apt="-o Acquire::Retries=3 -o Acquire::http::Timeout=30"
sudo DEBIAN_FRONTEND=noninteractive apt-get $apt -qq update
sudo DEBIAN_FRONTEND=noninteractive apt-get $apt -qq install -y --no-install-recommends build-essential pkg-config >/dev/null
case "$(uname -m)" in
  aarch64) a=arm64 sum=daa8679ba9615a74d2d0cec321dc593f21948a2a11bb65862b063d8b930f4bcb
    n=arm64 nsum=33c37be234ba8bee041961047b19160f4d7d0f164e8e246f78bdd10feff148b3 ;;
  x86_64) a=x86_64 sum=0191d6f1f3b716b5c55820032605d05fc3c1cdbf581ebeff655019e5dd1524c0
    n=x64 nsum=eb6728e77c576a35162c122c45ccadf224d89ff00abc8dae46cc2897c6002d7c ;;
  *) echo "no act build for $(uname -m)"; exit 1 ;;
esac
curl -fsSL --retry 3 -o /tmp/act.tgz "https://github.com/nektos/act/releases/download/v0.2.89/act_Linux_$a.tar.gz"
echo "$sum  /tmp/act.tgz" | sha256sum -c -
sudo tar -xzf /tmp/act.tgz -C /usr/local/bin act
curl -fsSL --retry 3 -o /tmp/node.tgz "https://nodejs.org/dist/v24.22.1/node-v24.22.1-linux-$n.tar.gz"
echo "$nsum  /tmp/node.tgz" | sha256sum -c -
sudo tar -xzf /tmp/node.tgz -C /usr/local --strip-components=1
sudo mkdir -p /home/runner
sudo chown "$(id -u):$(id -g)" /home/runner
mkdir -p "$HOME/w" "$HOME/sc"
"#;

/// The exit code of the job script when the VM failed before act started
const NOT_STARTED: i32 = 125;

/**
 * `gone`, which deletes a profile's VM and then its data disk. colima makes the disk before the VM,
 * and when VZ fails to create the VM its delete ignores the missing VM and keeps the disk. colima's
 * home is `$COLIMA_HOME`, else `~/.colima` when it exists, else `~/.config/colima`
 **/
macro_rules! gone {
    () => {
        r#"gone() {
  colima delete --force --data --profile "$1" || return 1
  h=${COLIMA_HOME:-$HOME/.colima}
  [ -n "${COLIMA_HOME:-}" ] || [ -d "$h" ] || h=$HOME/.config/colima
  LIMA_HOME=$h/_lima limactl disk delete "colima-$1"
}
"#
    };
}

/**
 * Deletes the VM and disk a dead run or a failed start left at this profile, which its job could
 * have changed, then starts the VM, sets it up, and streams the checkout's tracked files as they
 * are on disk, which hold the base's workflows, with its `.git`. The tracked files are read from
 * disk, never `git archive`, which a commit's `export-subst` rewrites. act reads the secrets once
 * from a FIFO, which then goes
 **/
const JOB: &str = concat!(
    gone!(),
    r#"p=$1 dir=$2 wf=$3 secrets=$4 cpus=$5 st=$6 actions=$7
shift 7
vm() { colima ssh --profile "$p" -- "$@"; }
listed() { colima list -j 2>/dev/null | grep -q "\"name\":\"$p\""; }
if ! gone "$p" || listed; then
  echo "shipcheck: the VM $p a dead run left would not delete"
  exit 125
fi
# VZ sometimes fails to create a new VM, and a new one after a delete starts
n=1
until colima start --profile "$p" --mount none --port-forwarder none --activate=false \
  --ssh-config=false --ssh-agent=false --template=false --save-config=false --runtime docker \
  --cpus "$cpus" --memory 8; do
  [ "$n" -lt 3 ] || exit 125
  n=$((n + 1))
  echo "shipcheck: the VM $p did not start, deleting it for start $n of 3"
  gone "$p" || exit 125
done
vm sh -c "$SETUP" || exit 125
# POSIX sh has no pipefail, so tar's status comes back through a file
{ (cd "$dir" && git ls-files -z > "$st.list" && printf '.git\0.github/workflows\0' >> "$st.list" &&
  COPYFILE_DISABLE=1 tar --null -T "$st.list" --no-xattrs -cf -); echo "$?" > "$st"; } |
  vm sh -c 'tar -x -C "$HOME/w"' || exit 125
[ "$(cat "$st")" = 0 ] || exit 125
# Writable, since a post step copies the action over its read-only git packs again. A copy that
# fails only costs act a clone, which it makes of any action it lacks
(cd "$actions" && COPYFILE_DISABLE=1 tar --no-xattrs -cf - .) |
  vm sh -c 'mkdir -p "$HOME/.cache/act" && tar -x -C "$HOME/.cache/act" && chmod -R u+w "$HOME/.cache/act"'
vm sh -c 'cat > "$HOME/sc/job.yml"' < "$wf" || exit 125
vm sh -c 'mkfifo -m 600 "$HOME/sc/secrets"' || exit 125
{ vm sh -c 'cat > "$HOME/sc/secrets"' < "$secrets"; vm sh -c 'rm -f "$HOME/sc/secrets"'; } &
vm sh -c 'cd "$HOME/w" && exec act "$@" --secret-file ../sc/secrets' sh "$@"
"#
);

/**
 * Deletes the VM unless shipcheck said `done` first, and kills the job's group only when shipcheck
 * never said `exited`, since after that the group id can belong to another process
 **/
const REAPER: &str = concat!(
    gone!(),
    r#"read -r pgid || pgid=
read -r state || state=
[ "$state" = done ] && exit 0
if [ "$state" = exited ]; then
  read -r state && [ "$state" = done ] && exit 0
elif [ -n "$pgid" ]; then
  kill -s KILL -- "-$pgid" 2>/dev/null
fi
# The locks this reaper holds stay out of colima's processes, which could outlive it
closed=
for fd in $LOCK_FDS; do closed="$closed $fd>&-"; done
eval "gone \"\$1\" $closed" >/dev/null 2>&1 &
d=$!
n=0
limit=${DELETE_SECS:-120}
while kill -0 "$d" 2>/dev/null && [ "$n" -lt "$limit" ]; do sleep 1; n=$((n + 1)); done
if ! kill -0 "$d" 2>/dev/null; then
  wait "$d" && rm -f "$2"
else
  # Its own group, which shipcheck started it in, so colima's limactl goes too, and this reaper last
  kill -s KILL 0
fi
"#
);

/// Variables colima and lima read, and shipcheck's safe git config, nothing else of this user's
fn clean_env(cmd: &mut Command) -> &mut Command {
    let keep = |k: &str| {
        ["PATH", "HOME", "USER", "TMPDIR"].contains(&k)
            || ["COLIMA_", "LIMA_", "GIT_CONFIG_"]
                .iter()
                .any(|p| k.starts_with(p))
    };
    cmd.env_clear()
        .envs(std::env::vars().filter(|(k, _)| keep(k)))
}

/// The VM's profile for one job checkout, which a slot's lock holds for one run at a time
pub fn profile(run_dir: &Path) -> String {
    let id = cache::hex(&Sha256::digest(run_dir.to_string_lossy().as_bytes()));
    format!("sc-{}", &id[..12])
}

/// VMs that run at once, one per 16 GB of memory, since each VM takes 8 and the Mac keeps the rest
fn vm_slots() -> u64 {
    let gb = first_line("sysctl", &["-n", "hw.memsize"])
        .ok()
        .and_then(|b| b.parse::<u64>().ok())
        .map_or(0, |b| b >> 30);
    (gb / 16).max(1)
}

fn first_line(program: &str, args: &[&str]) -> Result<String, String> {
    let out = clean_env(&mut Command::new(program))
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().next().unwrap_or_default().trim().to_string())
}

/// `colima version`'s first line, or why it failed
pub fn version() -> Result<String, String> {
    first_line("colima", &["version"])
}

fn delete(profile: &str) -> bool {
    clean_env(&mut Command::new("sh"))
        .args(["-c", concat!(gone!(), r#"gone "$1""#), "sh", profile])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

fn docker_context() -> Option<String> {
    first_line("docker", &["context", "show"]).ok()
}

/// What an outcome says of a job that ran out of time before act started
fn timed_out() -> Outcome {
    Outcome {
        exit: None,
        skipped: false,
        unsupported_jobs: Vec::new(),
        outputs: Default::default(),
        shards: None,
        summary: String::new(),
    }
}

/// Whether a name is one `profile` gives, `sc-` and 12 lowercase hex digits, and no one else's
fn is_profile(name: &str) -> bool {
    name.strip_prefix("sc-").is_some_and(|h| {
        h.len() == 12
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Where this home records each VM it starts until the VM is deleted
fn records() -> std::path::PathBuf {
    crate::run::home().join("colima")
}

/// The VMs this home started and has not seen deleted, which colima still lists
fn ours(recorded: &[String], listed: &[String]) -> Vec<String> {
    recorded
        .iter()
        .filter(|n| is_profile(n) && listed.contains(*n))
        .cloned()
        .collect()
}

/**
 * Deletes the VMs this home started that are still there, while no colima job runs here, which
 * the VM lock proves. A profile another home or another tool made, such as `sc-bench`, is never
 * recorded here, so it is never touched
 **/
fn leftovers() -> Vec<String> {
    let recorded: Vec<String> = fs::read_dir(records())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();

    if recorded.is_empty() {
        return Vec::new();
    }

    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::run::home().join("locks").join("colima.lock"));

    let Ok(lock) = lock else {
        return Vec::new();
    };

    if lock.try_lock().is_err() {
        return Vec::new();
    }

    let listed = profiles();
    let mut done = Vec::new();

    // A VM colima no longer lists can still have the disk of a start that failed
    for name in recorded.iter().filter(|n| !listed.contains(*n)) {
        if !is_profile(name) || delete(name) {
            let _ = fs::remove_file(records().join(name));
        }
    }

    for name in ours(&recorded, &listed) {
        if delete(&name) {
            let _ = fs::remove_file(records().join(&name));
            done.push(format!("removed the colima VM {name}"));
        }
    }

    done
}

/// The profiles colima has, from `colima list -j`, one object per line
fn profiles() -> Vec<String> {
    let Ok(out) = clean_env(&mut Command::new("colima"))
        .args(["list", "-j"])
        .output()
    else {
        return Vec::new();
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["name"].as_str().map(String::from))
        .collect()
}

/**
 * The act arguments inside the VM, with the labels the docker engine has an image for, and
 * `self-hosted` and `linux`, mapped to the VM itself. Any other label, such as a macOS leg, is
 * unsupported as it is on docker, so it fails rather than pass on Linux
 **/
fn act_args(request: &RunRequest, labels: Vec<String>) -> Vec<String> {
    let mut args: Vec<String> = [
        "workflow_dispatch",
        "-W",
        "../sc/job.yml",
        "-j",
        request.job,
        "--artifact-server-path",
        "../sc/artifacts",
        "--artifact-server-addr",
        "127.0.0.1",
        "--no-cache-server",
        "-s",
        "GITHUB_TOKEN=",
        "--env-file",
        "/dev/null",
        "--var-file",
        "/dev/null",
        "--input-file",
        "/dev/null",
    ]
    .map(String::from)
    .to_vec();
    let vm = ["self-hosted", "linux"].map(String::from);
    let mut labels: Vec<String> = labels.into_iter().chain(vm).collect();
    labels.sort();
    labels.dedup();

    for label in labels {
        args.extend(["-P".into(), format!("{label}=-self-hosted")]);
    }

    args
}

/**
 * Whether act, in a dry run on the Mac, says the job's `if:` skips it, so a skipped job boots no VM.
 * The run also fetches the job's actions into its cache on the Mac, which only act here writes and
 * the VM gets a copy of, so a job never changes what a later one runs. The dry run starts no
 * container, so the image is a placeholder, and labels go to it and never to `-self-hosted`, which
 * would set up the job on the Mac itself
 **/
fn skipped_by_if(act: &Act, request: &RunRequest, workflow: &str, actions: &Path) -> bool {
    let has_if = crate::workflow::load(workflow)
        .is_ok_and(|root| !yaml_rust2::Yaml::Hash(root)["jobs"][request.job]["if"].is_badvalue());
    let mut cmd = Command::new("act");
    cmd.arg("workflow_dispatch")
        .arg("-W")
        .arg(request.workflow)
        .args(["-j", request.job, "--pull=false", "--no-cache-server"])
        .args(["--container-daemon-socket", "-", "--network", "none"])
        .args(["-s", "GITHUB_TOKEN=", "--secret-file", "/dev/null"])
        .args(["--env-file", "/dev/null", "--var-file", "/dev/null"])
        .args(["--input-file", "/dev/null"])
        .arg("--action-cache-path")
        .arg(actions)
        .current_dir(request.run_dir);

    if act::cached_actions_resolve(actions) {
        cmd.arg("--action-offline-mode");
    }

    for label in act
        .labels(request.file)
        .into_iter()
        .chain(["self-hosted".into(), "linux".into()])
    {
        cmd.arg("-P").arg(format!("{label}=shipcheck/dry-run"));
    }

    // It runs without an `if:` too, to fill the cache
    act::skipped_by_if(clean_env(&mut cmd)) && has_if
}

/// act's verbose dry run of the job in its VM, which says whether act skipped it
fn dry_run(profile: &str, args: &[String]) -> String {
    clean_env(&mut Command::new("colima"))
        .args(["ssh", "--profile", profile, "--", "sh", "-c"])
        .arg(r#"cd "$HOME/w" && exec act "$@" --secret-file /dev/null --dryrun --verbose"#)
        .arg("sh")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

impl Engine for Colima {
    fn name(&self) -> &'static str {
        "colima"
    }

    /// colima and lima's versions, which pick the VM's default image, and the setup it runs
    fn identity(&self) -> Result<String, String> {
        let colima = version()?;
        let lima = first_line("limactl", &["--version"])?;
        let setup = cache::hex(&Sha256::digest(format!("{SETUP}{JOB}")));
        Ok(format!("{colima}, {lima}, setup {}", &setup[..16]))
    }

    fn preflight(&self) -> Result<(), String> {
        self.identity().map(|_| ())
    }

    fn run(&self, act: &Act, request: &RunRequest, log_path: &Path) -> Result<Outcome, String> {
        // The job's time starts now, waiting for a VM slot too
        let deadline = Instant::now() + request.timeout;
        let run_dir = request.run_dir;
        let isolated = act::isolated(request, &act::run_tag(run_dir), false)?;
        fs::write(request.workflow, &isolated).map_err(|e| e.to_string())?;

        let actions = act::job_actions(request)?;

        if skipped_by_if(act, request, &isolated, &actions) {
            fs::write(
                log_path,
                "shipcheck: the job's `if:` skips it, so no VM started\n",
            )
            .map_err(|e| e.to_string())?;
            return Ok(Outcome {
                exit: Some(0),
                skipped: true,
                ..timed_out()
            });
        }

        let artifacts = crate::workflow::job_runs(&isolated)
            .into_iter()
            .flat_map(|j| j.steps)
            .filter_map(|s| s.uses)
            .find(|u| {
                let action = u.split('@').next().unwrap_or(u);
                ["actions/upload-artifact", "actions/download-artifact"].contains(&action)
            });

        if let Some(uses) = artifacts {
            return Err(format!(
                "a colima job cannot pass artifacts to or from other jobs yet, and it uses {uses}"
            ));
        }

        let home = crate::run::home().join("locks");
        fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        let open = |name: String| {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(home.join(name))
                .map_err(|e| e.to_string())
        };
        let profile = profile(run_dir);
        // Held until this profile's VM is deleted, so a retry never starts it while it still goes
        let named = open(format!("colima-{profile}.lock"))?;
        // Shared by every colima job, so the leftover cleanup, which takes it alone, waits for all
        let jobs = open("colima.lock".into())?;
        let mut slot = None;

        while slot.is_none() {
            if named.try_lock().is_ok() && jobs.try_lock_shared().is_ok() {
                slot = (0..vm_slots())
                    .map(|i| open(format!("colima-vm-{i}.lock")))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .find(|f| f.try_lock().is_ok());
            }

            if slot.is_some() {
                break;
            }

            let _ = jobs.unlock();
            let _ = named.unlock();

            if Instant::now() > deadline {
                fs::write(
                    log_path,
                    format!(
                        "shipcheck: timed out waiting for another colima job's VM, all {} in use, or its cleanup\n",
                        vm_slots()
                    ),
                )
                .map_err(|e| e.to_string())?;
                return Ok(timed_out());
            }

            std::thread::sleep(Duration::from_millis(500));
        }

        let slot = slot.expect("a VM slot");
        let mut args = act_args(request, act.labels(request.file));

        // The Mac's act just fetched every action, so the VM's never asks GitHub for one it has
        if act::cached_actions_resolve(&actions) {
            args.push("--action-offline-mode".into());
        }
        let secrets = crate::secret::read(&crate::run::secrets_file(request.repo_id))?;
        let secrets_copy = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        fs::write(secrets_copy.path(), secrets.unwrap_or_default()).map_err(|e| e.to_string())?;
        let status = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        // Split between the VMs that may run at once, since each compile in an 8 GB VM takes memory
        let cpus = std::thread::available_parallelism().map_or(2, |n| {
            (n.get().saturating_sub(1) as u64 / vm_slots()).max(2)
        });
        let log = fs::File::create(log_path).map_err(|e| e.to_string())?;
        let context = docker_context();

        // Started first, so a shipcheck killed while the VM boots still leaves no VM. It holds the
        // slot's and the VM's locks, so no new run takes either while it deletes the VM
        let mut reaper = Command::new("sh");
        let fds: Vec<_> = request
            .slot_lock
            .into_iter()
            .chain([&named, &jobs, &slot].map(std::os::fd::AsRawFd::as_raw_fd))
            .collect();
        let held = super::inherit(&mut reaper, fds);
        let record = records().join(&profile);
        fs::create_dir_all(records()).map_err(|e| e.to_string())?;
        fs::write(&record, "").map_err(|e| e.to_string())?;
        let mut reaper = clean_env(&mut reaper)
            .env(
                "LOCK_FDS",
                held.iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .args(["-c", REAPER, "sh", &profile])
            .arg(&record)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("sh: {e}"))?;
        let mut cmd = Command::new("sh");
        clean_env(&mut cmd);
        cmd.args(["-c", JOB, "sh", &profile])
            .arg(run_dir)
            .arg(request.workflow)
            .arg(secrets_copy.path())
            .arg(cpus.to_string())
            .arg(status.path())
            .arg(&actions)
            .args(&args)
            .env("SETUP", SETUP)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .process_group(0);
        let spawned = cmd.spawn().map_err(|e| format!("sh: {e}"));
        let exit = spawned.and_then(|mut child| {
            if let Some(stdin) = reaper.stdin.as_mut() {
                let _ = writeln!(stdin, "{}", child.id());
            }

            // `exited` goes before the child is reaped, while no other process can have its id
            let mut exited = || {
                if let Some(stdin) = reaper.stdin.as_mut() {
                    let _ = writeln!(stdin, "exited");
                }
            };

            loop {
                let ended = super::has_exited(child.id());

                if matches!(ended, Ok(true)) {
                    exited();
                    break Ok(child.wait().map_err(|e| e.to_string())?.code());
                }

                // A child shipcheck cannot see end is stopped as on a timeout
                if ended.is_err() || Instant::now() > deadline {
                    // The group, so `colima ssh` and act's side go too
                    super::signal_group(child.id(), libc::SIGKILL);

                    while !super::has_exited(child.id()).unwrap_or(true) {
                        std::thread::sleep(Duration::from_millis(20));
                    }

                    exited();
                    let _ = child.wait();
                    break Ok(None);
                }

                std::thread::sleep(Duration::from_millis(200));
            }
        });

        let log = fs::read(log_path).map(|l| String::from_utf8_lossy(&l).into_owned());
        // Only a job that never reached its first step can be one act skipped, as on docker
        let dry = match (&exit, &log) {
            (Ok(Some(0)), Ok(log)) if !log.contains("shipcheck-start ") => dry_run(&profile, &args),
            _ => String::new(),
        };

        // The reaper deletes the VM after the result is back, holding its locks until it is gone
        drop(reaper.stdin.take());
        std::thread::spawn(move || reaper.wait());

        // `--activate=false` keeps the context, still one that colima switched to this VM goes back
        if let Some(before) = context.filter(|c| !c.starts_with("colima-sc-"))
            && docker_context().is_some_and(|now| now == format!("colima-{profile}"))
        {
            let _ = Command::new("docker")
                .args(["context", "use", &before])
                .output();
        }

        let exit = exit?;

        if exit == Some(NOT_STARTED) {
            return Err(format!(
                "the colima VM {profile} did not start or set up, see {}",
                log_path.display()
            ));
        }

        let log = log.map_err(|e| e.to_string())?;

        Ok(Outcome {
            exit,
            skipped: exit == Some(0)
                && !log.contains("shipcheck-start ")
                && act::dry_run_skips(&dry),
            unsupported_jobs: act::unsupported_jobs(&log),
            outputs: act::read_outputs(&log),
            shards: None,
            summary: act::read_summary(&log),
        })
    }

    /// A dead run's VM, found by the profile its checkout names, when this home recorded it
    fn reclaim(&self, run_dir: &Path) {
        let name = profile(run_dir);
        let record = records().join(&name);
        // A reaper still deleting the VM holds its lock, a dead run's VM has none
        let free = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(
                crate::run::home()
                    .join("locks")
                    .join(format!("colima-{name}.lock")),
            )
            .is_ok_and(|f| f.try_lock().is_ok());

        if record.exists() && free && delete(&name) {
            let _ = fs::remove_file(record);
        }
    }

    fn prune(&self) -> Vec<String> {
        leftovers()
    }

    fn clean(&self, _repo_id: Option<&str>) -> Vec<String> {
        leftovers()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_vms_this_home_recorded_are_ever_cleaned_up() {
        let mine = profile(Path::new("/runs/acme/0/ci.yml/e2e"));
        let other_home = profile(Path::new("/other/home/runs/acme/0/ci.yml/e2e"));
        let listed: Vec<String> = [&mine, &other_home, "sc-bench", "shipcheck-test", "default"]
            .map(String::from)
            .to_vec();
        // A record named like another tool's profile is not a profile shipcheck makes
        let recorded = vec![
            mine.clone(),
            "sc-bench".into(),
            "shipcheck-test".into(),
            "sc-ABCDEF123456".into(),
        ];
        assert_eq!(ours(&recorded, &listed), std::slice::from_ref(&mine));
        assert!(is_profile(&mine) && is_profile(&other_home));
        for name in [
            "sc-bench",
            "shipcheck-test",
            "sc-0123456789ab0",
            "sc-ABCDEF123456",
            "sc-",
        ] {
            assert!(!is_profile(name), "{name}");
        }
    }

    #[test]
    fn a_profile_is_per_checkout_and_never_another_tools() {
        let a = profile(Path::new("/runs/acme/0/ci.yml/e2e"));
        assert_eq!(a, profile(Path::new("/runs/acme/0/ci.yml/e2e")));
        assert_ne!(a, profile(Path::new("/runs/acme/1/ci.yml/e2e")));
        assert!(a.starts_with("sc-") && a.len() == 15, "{a}");
    }

    #[test]
    fn the_reaper_deletes_the_vm_only_when_shipcheck_dies_first() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("colima.log");

        for name in ["colima", "limactl"] {
            let fake = dir.path().join(name);
            fs::write(
                &fake,
                format!("#!/bin/sh\necho \"$*\" >> {}\n", log.display()),
            )
            .unwrap();
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path = format!("{}:/usr/bin:/bin", dir.path().display());

        // What shipcheck wrote before it died or ended, whether the VM goes, and whether the group is killed
        for (lines, deleted, killed) in [
            (&["1", "exited", "done"][..], false, false),
            (&["1", "exited"], true, false),
            (&["1"], true, true),
            (&[], true, false),
        ] {
            let _ = fs::remove_file(&log);
            let mut job = Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap();
            let mut reaper = Command::new(crate::runner::posix_sh())
                .args(["-c", REAPER, "sh", "sc-abc"])
                .env("PATH", &path)
                .env("COLIMA_HOME", dir.path())
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = reaper.stdin.take().unwrap();

            for line in lines {
                let line = if *line == "1" {
                    job.id().to_string()
                } else {
                    line.to_string()
                };
                writeln!(stdin, "{line}").unwrap();
            }

            drop(stdin);
            assert!(reaper.wait().unwrap().success());
            let calls = fs::read_to_string(&log).unwrap_or_default();

            if deleted {
                assert_eq!(
                    calls, "delete --force --data --profile sc-abc\ndisk delete colima-sc-abc\n",
                    "{lines:?}"
                );
            } else {
                assert_eq!(calls, "");
            }

            if killed {
                assert!(job.wait().unwrap().code().is_none(), "the job still runs");
            } else {
                assert!(
                    job.try_wait().unwrap().is_none(),
                    "{lines:?} killed the group"
                );
            }

            let _ = job.kill();
            let _ = job.wait();
        }
    }

    #[test]
    fn the_reaper_keeps_its_locks_from_colima_and_gives_up_on_a_hung_delete() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("colima.log");
        let child = dir.path().join("child.pid");
        // A lock at 10 or higher, which a shell cannot redirect
        let lock = crate::runner::tests::high(fs::File::create(dir.path().join("x.lock")).unwrap());
        let path = format!("{}:/usr/bin:/bin", dir.path().display());

        for (hang, kept) in [(false, false), (true, true)] {
            let record = dir.path().join("sc-abc");
            fs::write(&record, "").unwrap();
            let _ = fs::remove_file(&log);
            let mut cmd = Command::new(crate::runner::posix_sh());
            let held =
                super::super::inherit(&mut cmd, vec![std::os::fd::AsRawFd::as_raw_fd(&lock)]);
            assert_eq!(held, [9]);
            // It says its arguments and whether it got the lock, and when asked hangs in a child as limactl does
            let fake = format!(
                "#!/bin/sh\nif [ -e /dev/fd/9 ]; then echo \"open $*\" >> {log}; else echo \"closed $*\" >> {log}; fi\n{}",
                if hang {
                    format!("sleep 30 &\necho $! > {}\nwait\n", child.display())
                } else {
                    String::new()
                },
                log = log.display()
            );
            for name in ["colima", "limactl"] {
                fs::write(dir.path().join(name), &fake).unwrap();
                fs::set_permissions(dir.path().join(name), fs::Permissions::from_mode(0o755))
                    .unwrap();
            }

            // Its own group, as shipcheck starts it, since on a hung delete it kills its group
            let mut reaper = cmd
                .args(["-c", REAPER, "sh", "sc-abc"])
                .arg(&record)
                .env("PATH", &path)
                .env("COLIMA_HOME", dir.path())
                .env("LOCK_FDS", "9")
                // Long enough for a quick delete on a busy machine, short for a hung one
                .env("DELETE_SECS", if hang { "2" } else { "30" })
                .stdin(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap();
            let started = Instant::now();
            let status = reaper.wait().unwrap();
            assert_eq!(status.success(), !hang, "hang {hang}: {status}");
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "a hung delete held the reaper"
            );
            let disk = if hang {
                ""
            } else {
                "closed disk delete colima-sc-abc\n"
            };
            assert_eq!(
                fs::read_to_string(&log).unwrap(),
                format!("closed delete --force --data --profile sc-abc\n{disk}"),
                "colima or limactl got the lock or another argument"
            );
            assert_eq!(record.exists(), kept, "hang {hang}");

            if hang {
                let pid: i32 = fs::read_to_string(&child).unwrap().trim().parse().unwrap();
                let gone = || unsafe { libc::kill(pid, 0) } != 0;
                let deadline = Instant::now() + Duration::from_secs(5);

                while !gone() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }

                assert!(gone(), "the hung delete's child outlived the reaper");
            }
        }
    }

    #[test]
    fn the_vm_gets_node_24_from_nodejs_org_and_not_ubuntus_node_18() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let fakes = [
            ("sudo", "echo \"sudo $*\" >> \"$LOG\""),
            ("curl", "echo \"curl $*\" >> \"$LOG\""),
            ("sha256sum", "echo \"sum $(cat)\" >> \"$LOG\""),
            ("uname", "echo \"$ARCH\""),
        ];

        for (name, body) in fakes {
            let path = dir.path().join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        for (arch, node) in [("aarch64", "linux-arm64"), ("x86_64", "linux-x64")] {
            let _ = fs::remove_file(&log);
            let out = Command::new(crate::runner::posix_sh())
                .args(["-c", SETUP])
                .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
                .env("HOME", dir.path())
                .env("LOG", &log)
                .env("ARCH", arch)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let calls = fs::read_to_string(&log).unwrap();
            let apt = calls.lines().find(|l| l.contains("install")).unwrap();
            assert!(!apt.contains("nodejs"), "{apt}");
            let url = format!("https://nodejs.org/dist/v24.22.1/node-v24.22.1-{node}.tar.gz");
            assert!(calls.contains(&url), "{calls}");
            assert!(
                calls.lines().any(|l| l.starts_with("sum ")
                    && l.ends_with("  /tmp/node.tgz")
                    && l.len() == 83),
                "{calls}"
            );
        }
    }

    #[test]
    fn a_vm_that_fails_to_start_is_deleted_and_started_again_up_to_three_times() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (log, count) = (dir.path().join("colima.log"), dir.path().join("starts"));
        let path = format!("{}:/usr/bin:/bin", dir.path().display());

        // How many starts fail and the calls colima gets, with an ssh only once a start passed
        let limactl = dir.path().join("limactl");
        fs::write(
            &limactl,
            format!("#!/bin/sh\necho \"$1\" >> {}\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&limactl, fs::Permissions::from_mode(0o755)).unwrap();

        // Each start goes after a delete of the VM and its disk, which a failed start can leave alone
        for (fails, calls) in [
            (0, "delete disk list start ssh"),
            (
                2,
                "delete disk list start delete disk start delete disk start ssh",
            ),
            (
                3,
                "delete disk list start delete disk start delete disk start",
            ),
        ] {
            let _ = fs::remove_file(&log);
            let _ = fs::remove_file(&count);
            // ssh fails, so the job stops right after the start with the same code as a failed one
            let fake = format!(
                "#!/bin/sh\necho \"$1\" >> {log}\ncase \"$1\" in\n  start) n=$(($(cat {count} 2>/dev/null || echo 0) + 1)); echo \"$n\" > {count}; [ \"$n\" -gt {fails} ] ;;\n  list|delete) exit 0 ;;\n  *) exit 1 ;;\nesac\n",
                log = log.display(),
                count = count.display()
            );
            fs::write(dir.path().join("colima"), fake).unwrap();
            fs::set_permissions(dir.path().join("colima"), fs::Permissions::from_mode(0o755))
                .unwrap();
            let out = Command::new(crate::runner::posix_sh())
                .args(["-c", JOB, "sh", "sc-abc", "/nowhere", "wf", "secrets", "2"])
                .arg(dir.path().join("status"))
                .arg(dir.path().join("actions"))
                .env("PATH", &path)
                .env("COLIMA_HOME", dir.path())
                .env("SETUP", "true")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert_eq!(
                out.status.code(),
                Some(NOT_STARTED),
                "{fails} failed starts"
            );
            let got = fs::read_to_string(&log).unwrap();
            assert_eq!(got.split_whitespace().collect::<Vec<_>>().join(" "), calls);
        }
    }

    #[test]
    fn a_vm_a_failed_start_never_made_still_loses_its_disk() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (bin, home) = (dir.path().join("bin"), dir.path().join("home"));
        let log = dir.path().join("calls.log");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&home).unwrap();
        let limactl = format!("#!/bin/sh\necho \"$LIMA_HOME $*\" >> {}\n", log.display());
        let elsewhere = dir.path().join("elsewhere");
        let gone = |colima: &str, colima_home: Option<&Path>| {
            for (name, body) in [("colima", colima), ("limactl", &limactl)] {
                fs::write(bin.join(name), body).unwrap();
                fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
            }

            let _ = fs::remove_file(&log);
            let mut cmd = Command::new(crate::runner::posix_sh());
            cmd.args(["-c", concat!(gone!(), r#"gone "$1""#), "sh", "sc-abc"])
                .env_clear()
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .env("HOME", &home);

            if let Some(h) = colima_home {
                cmd.env("COLIMA_HOME", h);
            }

            let ok = cmd.status().unwrap().success();
            (ok, fs::read_to_string(&log).unwrap_or_default())
        };
        // As colima 0.10.3 does, it ignores a VM that does not exist and keeps the VM's disk
        let ignores = "#!/bin/sh\nexit 0\n";

        // colima's home is $COLIMA_HOME, else ~/.colima when it exists, else ~/.config/colima
        for (colima_home, dot, want) in [
            (None, false, home.join(".config/colima")),
            (None, true, home.join(".colima")),
            (Some(elsewhere.as_path()), true, elsewhere.clone()),
        ] {
            if dot {
                fs::create_dir_all(home.join(".colima")).unwrap();
            }

            let disk = format!("{}/_lima disk delete colima-sc-abc\n", want.display());
            assert_eq!(gone(ignores, colima_home), (true, disk));
        }

        // A VM colima could not delete may still use the disk, which stays
        assert_eq!(gone("#!/bin/sh\nexit 1\n", None), (false, String::new()));
    }

    #[test]
    fn maps_the_jobs_labels_to_the_vm() {
        let dir = tempfile::tempdir().unwrap();
        let none = Default::default();
        let request = RunRequest {
            run_dir: dir.path(),
            file: "ci.yml",
            workflow: dir.path(),
            job: "e2e",
            needs: &none,
            artifacts: dir.path(),
            repo_id: "acme/app",
            timeout: Duration::from_secs(1),
            shard: false,
            on_base: false,
            slot_lock: None,
        };
        let args = act_args(&request, vec!["ubuntu-latest".into()]);
        let mapped: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(a, _)| *a == "-P")
            .map(|(_, b)| b)
            .collect();
        // A macos-latest leg or a custom label gets no mapping, so act skips it and the job fails
        assert_eq!(
            mapped,
            [
                "linux=-self-hosted",
                "self-hosted=-self-hosted",
                "ubuntu-latest=-self-hosted"
            ]
        );
        assert!(
            !args.iter().any(|a| a.contains("secret")),
            "secrets go in through the FIFO only"
        );
        assert!(args.windows(2).any(|w| w == ["-j", "e2e"]));
        assert!(args.windows(2).any(|w| w == ["-s", "GITHUB_TOKEN="]));
    }
}
