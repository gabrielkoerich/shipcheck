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
