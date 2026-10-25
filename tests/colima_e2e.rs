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

// The job's own Docker, which a container job never has, and a file only the tested tree holds
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
        .filter(|n| n.starts_with("sc-"))
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

fn shipcheck(work: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_shipcheck"));
    cmd.args(["run", "--sha", "HEAD", "--no-fetch"])
        .current_dir(work)
        .env("SHIPCHECK_HOME", home)
        .env("SHIPCHECK_NO_DAEMON", "1");
    cmd
}

#[test]
#[ignore = "needs colima, act, Docker and the internet"]
fn a_colima_job_runs_in_its_own_vm_and_leaves_none_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let work = repo(tmp.path());
    let home = tmp.path().join("home");
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
    assert!(ours().is_empty(), "a VM was left: {:?}", ours());

    // Killed while the VM runs, the reaper still deletes it
    fs::write(work.join("marker.txt"), "again").unwrap();
    git(&work, &["commit", "-q", "-am", "again"]);
    let mut run = shipcheck(&work, &home).spawn().unwrap();
    let started = Instant::now();

    while ours().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "no VM started"
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    std::thread::sleep(Duration::from_secs(5));
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

    // Other profiles, such as a developer's own, are never touched
    for line in before.lines().skip(1) {
        let name = line.split_whitespace().next().unwrap_or_default();
        assert!(profiles().contains(name), "{name} is gone");
    }
}
