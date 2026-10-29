mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/**
 * The job's own Docker, which a container job never has, a file only the tested tree holds, and
 * what the VM must not have: the Mac's files, shipcheck's environment, the host's Docker or the
 * secrets file. With `slow.txt` it listens on a port the Mac must not see and waits
 **/
const CI: &str = "\
on:
  push:
    branches: [main]
jobs:
  e2e:
    runs-on: ubuntu-24.04-arm
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1
      - run: test -f marker.txt
      - run: docker run --rm busybox true
      - run: test \"$(cat .github/workflows/ci.yml | head -1)\" = \"on:\"
      - run: test ! -e /Users && test ! -e /Volumes
      - run: test -z \"${SHIPCHECK_CANARY:-}\" && ! env | grep -q canary-7f3a
      - run: test \"$(docker info --format '{{.Name}}')\" = \"$(hostname)\"
      - run: test ! -e \"$HOME/sc/secrets\"
      - run: node -e 'new File([], \"f\")' && test \"$(node -p process.versions.node | cut -d. -f1)\" -ge 24
      - run: if [ -f slow.txt ]; then python3 -u -m http.server 38517 & sleep 600; fi
";

const CONFIG: &str = "\
runner = \"host\"
retries = 0

[host.checks]
test = \"test -f marker.txt\"

[jobs.\"ci.yml/e2e\"]
runner = \"act\"
engine = \"colima\"
";

/// The `sc-` profiles colima has, which only shipcheck's colima engine makes
fn ours() -> Vec<String> {
    let out = Command::new("colima")
        .args(["list", "-j"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["name"].as_str().map(String::from))
        // A base stays for the next job, only a job's VM must go
        .filter(|n| n.starts_with("sc-") && !n.starts_with("sc-base-"))
        .collect()
}

fn profiles() -> String {
    String::from_utf8_lossy(&Command::new("colima").arg("list").output().unwrap().stdout).into()
}

fn repo(tmp: &Path) -> PathBuf {
    let origin = tmp.join("origin.git");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(tmp, &["clone", "-q", &origin.to_string_lossy(), "work"]);
    let work = tmp.join("work");
    git(&work, &["checkout", "-q", "-b", "main"]);
    fs::create_dir_all(work.join(".github/workflows")).unwrap();
    fs::write(work.join(".github/workflows/ci.yml"), CI).unwrap();
    fs::write(work.join("shipcheck.toml"), CONFIG).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("marker.txt"), "ok").unwrap();
    // A branch's own workflow never runs, the job sees the base's
    fs::write(work.join(".github/workflows/ci.yml"), "# changed\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "feature"]);
    work
}

/// Every file under `dir`
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();

    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();

        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }

    out
}

/// The records of job VMs in a home, without the base's, which stays
fn job_records(home: &Path) -> Vec<PathBuf> {
    walk(&home.join("colima"))
        .into_iter()
        .filter(|p| {
            !p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("sc-base-")
        })
        .collect()
}

fn shipcheck(work: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_shipcheck"));
    cmd.args(["run", "--sha", "HEAD", "--no-fetch"])
        .current_dir(work)
        .env("SHIPCHECK_HOME", home)
        .env("SHIPCHECK_NO_DAEMON", "1")
        .env("SHIPCHECK_CANARY", "canary-7f3a");
    cmd
}

#[test]
#[ignore = "needs colima, act, Docker and the internet"]
fn a_colima_job_runs_in_its_own_vm_and_leaves_none_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let work = repo(tmp.path());
    let home = tmp.path().join("home");
    let _clean = common::CleanOnDrop::new(&home);
    let before = profiles();

    let out = shipcheck(&work, &home).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("ci.yml/e2e: success"), "{text}");
    assert!(text.contains("host/test: success"), "{text}");
    // The reaper deletes the VM after the result is back
    let passed = Instant::now();

    while !ours().is_empty() || !job_records(&home).is_empty() {
        assert!(
            passed.elapsed() < Duration::from_secs(120),
            "a VM or its record was left: {:?}",
            ours()
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    // The base the job's VM was cloned from stays, stopped and ready for the next job
    let bases: Vec<_> = walk(&home.join("colima"))
        .into_iter()
        .filter(|p| fs::read_to_string(p).is_ok_and(|r| r == "ready"))
        .collect();
    assert_eq!(bases.len(), 1, "{bases:?}");

    // Killed while the VM runs, the reaper still deletes it
    fs::write(work.join("slow.txt"), "listen").unwrap();
    git(&work, &["add", "slow.txt"]);
    git(&work, &["commit", "-q", "-m", "slow"]);
    let mut run = shipcheck(&work, &home).spawn().unwrap();
    let started = Instant::now();

    while ours().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "no VM started"
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    // No port of the VM reaches the Mac's loopback, and no host ssh config names it
    let serving = || {
        walk(&home.join("logs"))
            .iter()
            .any(|f| fs::read_to_string(f).is_ok_and(|t| t.contains("Serving HTTP on")))
    };

    while !serving() {
        assert!(
            started.elapsed() < Duration::from_secs(600),
            "the job never listened"
        );
        std::thread::sleep(Duration::from_secs(1));
    }

    let addr = "127.0.0.1:38517".parse().unwrap();
    assert!(
        std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_err(),
        "the VM's port reached the Mac"
    );
    let ssh = std::env::home_dir().unwrap().join(".ssh/config");
    let vm = ours()[0].clone();
    assert!(
        !fs::read_to_string(ssh).unwrap_or_default().contains(&vm),
        "colima wrote {vm} into ~/.ssh/config"
    );

    run.kill().unwrap();
    let _ = run.wait();
    let killed = Instant::now();

    while !ours().is_empty() {
        assert!(
            killed.elapsed() < Duration::from_secs(120),
            "the VM outlived shipcheck: {:?}",
            ours()
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    // The reaper removes its record once the delete succeeds
    while !job_records(&home).is_empty() {
        assert!(
            killed.elapsed() < Duration::from_secs(130),
            "a record was left: {:?}",
            job_records(&home)
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // Other profiles, such as a developer's own, are never touched
    for line in before.lines().skip(1) {
        let name = line.split_whitespace().next().unwrap_or_default();
        assert!(profiles().contains(name), "{name} is gone");
    }
}
