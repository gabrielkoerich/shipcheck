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

/// The VM's packages, act pinned as ci.yml pins it, and GitHub's `/home/runner`. Part of the key.
const SETUP: &str = r#"set -eu
apt="-o Acquire::Retries=3 -o Acquire::http::Timeout=30"
sudo DEBIAN_FRONTEND=noninteractive apt-get $apt -qq update
sudo DEBIAN_FRONTEND=noninteractive apt-get $apt -qq install -y --no-install-recommends build-essential nodejs pkg-config >/dev/null
case "$(uname -m)" in
  aarch64) a=arm64 sum=daa8679ba9615a74d2d0cec321dc593f21948a2a11bb65862b063d8b930f4bcb ;;
  x86_64) a=x86_64 sum=0191d6f1f3b716b5c55820032605d05fc3c1cdbf581ebeff655019e5dd1524c0 ;;
  *) echo "no act build for $(uname -m)"; exit 1 ;;
esac
curl -fsSL --retry 3 -o /tmp/act.tgz "https://github.com/nektos/act/releases/download/v0.2.89/act_Linux_$a.tar.gz"
echo "$sum  /tmp/act.tgz" | sha256sum -c -
sudo tar -xzf /tmp/act.tgz -C /usr/local/bin act
sudo mkdir -p /home/runner
sudo chown "$(id -u):$(id -g)" /home/runner
mkdir -p "$HOME/w" "$HOME/sc"
"#;

/// The exit code of the job script when the VM failed before act started
const NOT_STARTED: i32 = 125;

/**
 * Deletes a VM a dead run left at this profile, whose disk its job could have changed, then starts
 * the VM, sets it up, and streams the checkout's tracked files as they are on disk, which hold the
 * base's workflows, with its `.git`. The tracked files are read from disk, never `git archive`,
 * which a commit's `export-subst` rewrites. act reads the secrets once from a FIFO, which then goes
 **/
const JOB: &str = r#"p=$1 dir=$2 wf=$3 secrets=$4 dry=$5 cpus=$6 st=$7
shift 7
vm() { colima ssh --profile "$p" -- "$@"; }
listed() { colima list -j 2>/dev/null | grep -q "\"name\":\"$p\""; }
if listed; then
  colima delete --force --data --profile "$p" || true
  if listed; then echo "shipcheck: the VM $p a dead run left would not delete"; exit 125; fi
fi
colima start --profile "$p" --mount none --port-forwarder none --activate=false \
  --ssh-config=false --ssh-agent=false --template=false --save-config=false --runtime docker \
  --cpus "$cpus" --memory 8 || exit 125
vm sh -c "$SETUP" || exit 125
# POSIX sh has no pipefail, so tar's status comes back through a file
{ (cd "$dir" && git ls-files -z > "$st.list" && printf '.git\0.github/workflows\0' >> "$st.list" &&
  COPYFILE_DISABLE=1 tar --null -T "$st.list" --no-xattrs -cf -); echo "$?" > "$st"; } |
  vm sh -c 'tar -x -C "$HOME/w"' || exit 125
[ "$(cat "$st")" = 0 ] || exit 125
vm sh -c 'cat > "$HOME/sc/job.yml"' < "$wf" || exit 125
vm sh -c 'mkfifo -m 600 "$HOME/sc/secrets"' || exit 125
{ vm sh -c 'cat > "$HOME/sc/secrets"' < "$secrets"; vm sh -c 'rm -f "$HOME/sc/secrets"'; } &
code=0
vm sh -c 'cd "$HOME/w" && exec act "$@" --secret-file ../sc/secrets' sh "$@" || code=$?
if [ "$code" = 0 ]; then
  vm sh -c 'cd "$HOME/w" && exec act "$@" --secret-file /dev/null --dryrun --verbose' sh "$@" > "$dry" 2>/dev/null || true
fi
exit "$code"
"#;

/**
 * Deletes the VM unless shipcheck said `done` first, and kills the job's group only when shipcheck
 * never said `exited`, since after that the group id can belong to another process
 **/
const REAPER: &str = r#"read -r pgid || pgid=
read -r state || state=
[ "$state" = done ] && exit 0
if [ "$state" = exited ]; then
  read -r state && [ "$state" = done ] && exit 0
elif [ -n "$pgid" ]; then
  kill -s KILL -- "-$pgid" 2>/dev/null
fi
colima delete --force --data --profile "$1" >/dev/null 2>&1
"#;

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
    clean_env(&mut Command::new("colima"))
        .args(["delete", "--force", "--data", "--profile", profile])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

fn docker_context() -> Option<String> {
    first_line("docker", &["context", "show"]).ok()
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
        let run_dir = request.run_dir;
        let isolated = act::isolated(request, &act::run_tag(run_dir), false)?;
        fs::write(request.workflow, &isolated).map_err(|e| e.to_string())?;
        let args = act_args(request, act.labels(request.file));
        let profile = profile(run_dir);
        let secrets = crate::secret::read(&crate::run::secrets_file(request.repo_id))?;
        let secrets_copy = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        fs::write(secrets_copy.path(), secrets.unwrap_or_default()).map_err(|e| e.to_string())?;
        let dry = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        let status = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        let home = crate::run::home().join("locks");
        fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        let one_vm = fs::File::create(home.join("colima.lock")).map_err(|e| e.to_string())?;
        one_vm.lock().map_err(|e| e.to_string())?;
        let cpus =
            std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(2));
        let log = fs::File::create(log_path).map_err(|e| e.to_string())?;
        let context = docker_context();

        // Started first, so a shipcheck killed while the VM boots still leaves no VM
        let mut reaper = clean_env(&mut Command::new("sh"))
            .args(["-c", REAPER, "sh", &profile])
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
            .arg(dry.path())
            .arg(cpus.to_string())
            .arg(status.path())
            .args(&args)
            .env("SETUP", SETUP)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .process_group(0);
        let spawned = cmd.spawn().map_err(|e| format!("sh: {e}"));
        let deadline = Instant::now() + request.timeout;
        let exit = spawned.and_then(|mut child| {
            if let Some(stdin) = reaper.stdin.as_mut() {
                let _ = writeln!(stdin, "{}", child.id());
            }

            loop {
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    break Ok(status.code());
                }

                if Instant::now() > deadline {
                    // The group, so `colima ssh` and act's side go too
                    let _ = Command::new("kill")
                        .args(["-s", "KILL", "--", &format!("-{}", child.id())])
                        .status();
                    let _ = child.wait();
                    break Ok(None);
                }

                std::thread::sleep(Duration::from_millis(200));
            }
        });

        // The group is gone, so the reaper must not kill by its id, which can be reused
        if let Some(stdin) = reaper.stdin.as_mut() {
            let _ = writeln!(stdin, "exited");
        }

        let deleted = delete(&profile);

        if let Some(stdin) = reaper.stdin.as_mut()
            && deleted
        {
            let _ = writeln!(stdin, "done");
        }

        drop(reaper.stdin.take());
        let _ = reaper.wait();

        // `--activate=false` keeps the context, still one that colima switched to this VM goes back
        if let Some(before) = context
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

        if !deleted {
            let mut log = fs::OpenOptions::new()
                .append(true)
                .open(log_path)
                .map_err(|e| e.to_string())?;
            let _ = writeln!(
                log,
                "shipcheck: could not delete the VM {profile}, a later run deletes it first"
            );
        }

        let log =
            String::from_utf8_lossy(&fs::read(log_path).map_err(|e| e.to_string())?).into_owned();
        let dry = fs::read_to_string(dry.path()).unwrap_or_default();

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

    /// A dead run's VM, found by the profile its checkout names
    fn reclaim(&self, run_dir: &Path) {
        let name = profile(run_dir);

        if profiles().contains(&name) {
            delete(&name);
        }
    }

    fn prune(&self) -> Vec<String> {
        Vec::new()
    }

    fn clean(&self, _repo_id: Option<&str>) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let colima = dir.path().join("colima");
        fs::write(
            &colima,
            format!("#!/bin/sh\necho \"$*\" >> {}\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&colima, fs::Permissions::from_mode(0o755)).unwrap();
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
                    calls, "delete --force --data --profile sc-abc\n",
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
