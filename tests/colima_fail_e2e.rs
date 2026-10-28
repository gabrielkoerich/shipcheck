mod common;

use std::fs;
use std::path::Path;
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

// Passes only with marker.txt, and with hang.txt outlives its five-minute timeout, which counts the VM's boot
const CI: &str = "\
on: [push, pull_request]
jobs:
  e2e:
    runs-on: ubuntu-24.04-arm
    steps:
      - run: test -f marker.txt
      - run: if [ -f hang.txt ]; then sleep 900; fi
";

const CONFIG: &str = "\
runner = \"act\"
workflows = [\"ci.yml\"]
retries = 0
timeout_minutes = 5

[jobs.\"ci.yml/e2e\"]
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

/// Passed results in the home's cache, which only a full run that passed writes
fn passes(home: &Path) -> usize {
    fs::read_dir(home.join("cache"))
        .map(|d| {
            d.filter_map(|e| fs::read(e.ok()?.path()).ok())
                .filter_map(|t| serde_json::from_slice::<serde_json::Value>(&t).ok())
                .filter(|e| e["passed"] == true)
                .count()
        })
        .unwrap_or(0)
}

fn no_vm_left() {
    let started = Instant::now();

    while !ours().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "a VM was left: {:?}",
            ours()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
#[ignore = "needs colima, act, Docker and the internet"]
fn a_colima_job_that_fails_or_times_out_fails_and_leaves_no_vm() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _clean = common::CleanOnDrop::new(&home);
    let origin = tmp.path().join("origin.git");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(
        tmp.path(),
        &["clone", "-q", &origin.to_string_lossy(), "work"],
    );
    let work = tmp.path().join("work");
    git(&work, &["checkout", "-q", "-b", "main"]);
    fs::create_dir_all(work.join(".github/workflows")).unwrap();
    fs::write(work.join(".github/workflows/ci.yml"), CI).unwrap();
    fs::write(work.join("shipcheck.toml"), CONFIG).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);

    let run = |files: &[&str]| {
        for f in files {
            fs::write(work.join(f), "ok").unwrap();
        }

        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "--allow-empty", "-m", "change"]);
        let out = Command::new(env!("CARGO_BIN_EXE_shipcheck"))
            .args(["run", "--sha", "HEAD", "--no-fetch"])
            .current_dir(&work)
            .env("SHIPCHECK_HOME", &home)
            .env("SHIPCHECK_NO_DAEMON", "1")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    };

    let (code, text) = run(&[]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("ci.yml/e2e: failure"), "{text}");
    no_vm_left();

    let (code, text) = run(&["marker.txt", "hang.txt"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("ci.yml/e2e: timed out"), "{text}");
    no_vm_left();
    assert_eq!(passes(&home), 0);
}

// GitHub skips `release` on every PR, and `coverage` passes artifacts, which a colima job cannot yet
const SKIP_CI: &str = "\
on: [push, pull_request]
jobs:
  release:
    if: github.ref == 'refs/heads/main'
    runs-on: ubuntu-24.04-arm
    steps:
      - run: exit 1
  coverage:
    runs-on: ubuntu-24.04-arm
    steps:
      - uses: actions/upload-artifact@v4
        with:
          name: coverage
          path: lcov.info
";

const SKIP_CONFIG: &str = "\
runner = \"act\"
workflows = [\"ci.yml\"]
retries = 0

[jobs.\"ci.yml/release\"]
engine = \"colima\"

[jobs.\"ci.yml/coverage\"]
engine = \"colima\"
";

#[test]
#[ignore = "needs colima, act and Docker"]
fn a_skipped_colima_job_boots_no_vm_and_one_shipcheck_cannot_run_still_gets_a_log() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let origin = tmp.path().join("origin.git");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(
        tmp.path(),
        &["clone", "-q", &origin.to_string_lossy(), "work"],
    );
    let work = tmp.path().join("work");
    git(&work, &["checkout", "-q", "-b", "main"]);
    fs::create_dir_all(work.join(".github/workflows")).unwrap();
    fs::write(work.join(".github/workflows/ci.yml"), SKIP_CI).unwrap();
    fs::write(work.join("shipcheck.toml"), SKIP_CONFIG).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "change"]);

    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_shipcheck"))
        .args(["run", "--sha", "HEAD", "--no-fetch"])
        .current_dir(&work)
        .env("SHIPCHECK_HOME", &home)
        .env("SHIPCHECK_NO_DAEMON", "1")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("ci.yml/release: skipped"), "{text}");
    assert!(
        text.contains("ci.yml/coverage: failure, shipcheck could not run it"),
        "{text}"
    );
    // A VM takes 45 s or more to boot and delete
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "a VM booted: {text}"
    );
    no_vm_left();

    let logs: Vec<_> = walkdir(&home.join("logs"))
        .into_iter()
        .filter(|p| p.ends_with("ci.yml/coverage.log"))
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    let log = fs::read_to_string(&logs[0]).unwrap();
    assert!(log.contains("cannot pass artifacts"), "{log}");
}

/// Every file under `dir`
fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();

    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();

        if path.is_dir() {
            out.extend(walkdir(&path));
        } else {
            out.push(path);
        }
    }

    out
}
