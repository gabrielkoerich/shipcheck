#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The lines a command prints, none when it is missing or fails
fn lines(cmd: &str, args: &[&str]) -> BTreeSet<String> {
    let Ok(out) = Command::new(cmd).args(args).output() else {
        return BTreeSet::new();
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The account names of shipcheck's keychain items, read without their secrets
fn keychain_accounts() -> BTreeSet<String> {
    if !cfg!(target_os = "macos") {
        return BTreeSet::new();
    }

    let out = Command::new("security")
        .arg("dump-keychain")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut found = BTreeSet::new();

    // Each item starts with a `keychain:` line, and its attributes follow
    for item in out.split("keychain: ") {
        let attr = |key: &str| {
            item.lines()
                .find_map(|l| l.trim().strip_prefix(&format!("\"{key}\"<blob>=\"")))
                .and_then(|v| v.strip_suffix('"'))
        };

        if attr("svce") == Some("shipcheck") {
            found.insert(attr("acct").unwrap_or_default().to_string());
        }
    }

    found
}

/**
 * What a test could leave outside its temp dirs and Docker: shipcheck's keychain accounts, colima
 * profiles and shipcheck's launchd jobs. `docker_of` finds a test's Docker resources by name
 * instead, since tests that run at once each make their own
 **/
pub fn outside() -> BTreeMap<&'static str, BTreeSet<String>> {
    let colima = lines("colima", &["list"])
        .iter()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next().map(String::from))
        .collect();
    let launchd = lines("launchctl", &["list"])
        .iter()
        .filter_map(|l| l.split_whitespace().nth(2))
        .filter(|label| label.starts_with("run.shipcheck."))
        .map(String::from)
        .collect();

    BTreeMap::from([
        ("keychain accounts", keychain_accounts()),
        ("colima profiles", colima),
        ("launchd jobs", launchd),
    ])
}

/// What `after` has that `before` did not
pub fn new_since(
    before: &BTreeMap<&'static str, BTreeSet<String>>,
    after: &BTreeMap<&'static str, BTreeSet<String>>,
) -> BTreeMap<&'static str, Vec<String>> {
    after
        .iter()
        .map(|(kind, now)| {
            let was = before.get(kind).cloned().unwrap_or_default();
            (*kind, now.difference(&was).cloned().collect::<Vec<_>>())
        })
        .filter(|(_, new)| !new.is_empty())
        .collect()
}

/// Every lock file under `dir`, without following links
fn locks(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();

    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };

        if kind.is_dir() {
            found.extend(locks(&entry.path()));
        } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "lock") {
            found.push(entry.path());
        }
    }

    found
}

/// Waits until no run of the home holds a lock, as a run a hook started in the background can
fn wait_for_runs(home: &Path) {
    let deadline = Instant::now() + Duration::from_secs(600);
    let held = || {
        locks(home)
            .iter()
            .any(|lock| std::fs::File::open(lock).is_ok_and(|f| f.try_lock().is_err()))
    };

    while held() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Whether a host check's launchd job belongs to another home, the user's daemon's or a test's that
/// runs beside this one. Its script in the temp dir names the home whose logs it writes
fn anothers_check(label: &str, home: &Path) -> bool {
    let Some(id) = label.strip_prefix("run.shipcheck.check.") else {
        return false;
    };
    // A job whose script is gone ended, so it is no one's leftover
    let script = std::env::temp_dir().join(format!("shipcheck-job-{id}/job.sh"));
    let text = std::fs::read_to_string(script).unwrap_or_default();
    let real = home.canonicalize().unwrap_or_default();

    !text.contains(&*home.to_string_lossy())
        && (real.as_os_str().is_empty() || !text.contains(&*real.to_string_lossy()))
}

/// Waits for the background runs of the home, also when an assertion fails, so none outlives the test
pub struct WaitOnDrop(pub PathBuf);

impl Drop for WaitOnDrop {
    fn drop(&mut self) {
        wait_for_runs(&self.0);
    }
}

/**
 * What the runs of `home` left in Docker, by name: job containers that mount a path of the home
 * and the volume act keeps beside each, act's volumes of the home's run tags, the volumes and
 * networks named with the home's prefix, and an `act-toolcache` that `before` did not have
 **/
fn docker_of(home: &Path, before: &BTreeSet<String>) -> Vec<(&'static str, String)> {
    let prefix = shipcheck::runner::act::prefix_of(home);
    let homes: Vec<String> = [home.to_path_buf(), home.canonicalize().unwrap_or_default()]
        .iter()
        .map(|h| h.to_string_lossy().into_owned())
        .filter(|h| !h.is_empty())
        .collect();
    let mut found = Vec::new();

    for id in lines("docker", &["ps", "-aq", "--filter", "name=^act-"]) {
        let format = "{{.Name}}{{range .Mounts}} {{.Source}}{{end}}";
        let mounts = lines("docker", &["inspect", "-f", format, &id]);
        let Some(line) = mounts.iter().next() else {
            continue;
        };

        if homes.iter().any(|h| line.contains(h.as_str())) {
            let name = line.split_whitespace().next().unwrap_or_default();
            found.push(("container", id.clone()));
            found.push(("volume", format!("{}-env", name.trim_start_matches('/'))));
        }
    }

    let tags = shipcheck::runner::act::run_tags(home);

    for v in lines("docker", &["volume", "ls", "-q"]) {
        let act_env = v.starts_with("act-")
            && v.ends_with("-env")
            && tags.iter().any(|t| v.contains(&format!("-{t}-")));
        // act makes its shared tool cache when it is missing, which only this test then used
        let toolcache = v == "act-toolcache" && !before.contains(&v);

        if v.starts_with(&prefix) || act_env || toolcache {
            found.push(("volume", v));
        }
    }

    for n in lines("docker", &["network", "ls", "--format", "{{.Name}}"]) {
        if n.starts_with(&prefix) {
            found.push(("network", n));
        }
    }

    found
}

/// Removes what `docker_of` finds, containers first, since Docker keeps what a container uses.
/// Never a prune or `shipcheck clean`
fn remove_docker(home: &Path, before: &BTreeSet<String>) {
    for (kind, name) in docker_of(home, before) {
        let args: &[&str] = match kind {
            "container" => &["rm", "-f", "-v", &name],
            "volume" => &["volume", "rm", &name],
            _ => &["network", "rm", &name],
        };
        let _ = Command::new("docker").args(args).output();
    }
}

/**
 * Removes what the test's runs left in Docker, even when an assertion fails, and then fails the test
 * if `docker_of` still finds any, or anything `outside` lists is new. A run a hook started in the
 * background can still hold a slot, so it waits for that run first. Declare it after the temp dir,
 * so it drops first and cleans while the home still exists
 **/
pub struct CleanOnDrop {
    home: PathBuf,
    before: BTreeMap<&'static str, BTreeSet<String>>,
    volumes: BTreeSet<String>,
}

impl CleanOnDrop {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            before: outside(),
            volumes: lines("docker", &["volume", "ls", "-q"]),
        }
    }
}

impl Drop for CleanOnDrop {
    fn drop(&mut self) {
        wait_for_runs(&self.home);
        remove_docker(&self.home, &self.volumes);

        // A VM this home's runs started has a record under `colima` until it is deleted
        for vm in &outside()["colima profiles"] {
            if vm.starts_with("sc-")
                && !self.before["colima profiles"].contains(vm)
                && self.home.join("colima").join(vm).exists()
            {
                let _ = Command::new("colima")
                    .args(["delete", "--force", "--data", vm])
                    .output();
                let lima = std::env::home_dir().unwrap().join(".colima/_lima");
                let _ = Command::new("limactl")
                    .args(["disk", "delete", &format!("colima-{vm}")])
                    .env("LIMA_HOME", lima)
                    .output();
            }
        }

        let mut left = new_since(&self.before, &outside());

        if let Some(jobs) = left.get_mut("launchd jobs") {
            jobs.retain(|label| !anothers_check(label, &self.home));
        }

        // The user's daemon starts and deletes VMs of its own while a test runs
        if let Some(vms) = left.get_mut("colima profiles") {
            vms.retain(|vm| self.home.join("colima").join(vm).exists());
        }

        left.insert(
            "docker",
            docker_of(&self.home, &self.volumes)
                .into_iter()
                .map(|(kind, name)| format!("{kind} {name}"))
                .collect(),
        );
        left.retain(|_, new| !new.is_empty());

        if std::thread::panicking() {
            if !left.is_empty() {
                eprintln!("the test left {left:?}");
            }

            return;
        }

        assert!(left.is_empty(), "the test left {left:?}");
    }
}
