//! The commands the git hooks call, through the real binary with a home of their own

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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

/// Every record line under the home's `background/`, from any repository and branch
fn records(home: &Path) -> String {
    let mut text = String::new();
    let mut dirs = vec![home.join("background")];

    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                dirs.push(entry.path());
            } else {
                text += &fs::read_to_string(entry.path()).unwrap_or_default();
            }
        }
    }

    text
}

#[test]
fn on_push_runs_the_pushed_commit_and_on_commit_only_when_asked() {
    // A shipcheck host check runs this test jailed, and macOS refuses a sandbox inside a sandbox
    if std::env::var_os("SHIPCHECK_JAIL").is_some() {
        return;
    }

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
    git(&work, &["config", "user.email", "t@example.com"]);
    git(&work, &["config", "user.name", "t"]);
    fs::write(
        work.join("shipcheck.toml"),
        "runner = \"host\"\nretries = 0\n[host]\nchecks = { ok = \"true\" }\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "config"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    git(&work, &["switch", "-q", "-c", "feature"]);
    fs::write(work.join("a.txt"), "a").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "a"]);
    let sha = git(&work, &["rev-parse", "HEAD"]);

    let shipcheck = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_shipcheck"))
            .args(args)
            .env("SHIPCHECK_HOME", &home)
            .env("SHIPCHECK_NO_DAEMON", "1")
            .current_dir(&work)
            .output()
            .unwrap()
    };

    // The pre-push hook's call returns at once and leaves a run of the pushed commit
    let pushed = shipcheck(&["on-push", "refs/heads/feature", &sha]);
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(120);

    while !shipcheck(&["status", &sha]).status.success() {
        assert!(
            Instant::now() < deadline,
            "on-push must start a run that caches a result. Records:\n{}\nLog:\n{}",
            records(&home),
            fs::read_to_string(home.join("logs/background.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // With the default `on = ["push"]` a commit starts nothing
    fs::write(work.join("b.txt"), "b").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "b"]);
    let committed = git(&work, &["rev-parse", "HEAD"]);
    assert!(shipcheck(&["on-commit"]).status.success());
    assert!(
        !records(&home).contains(&committed),
        "on-commit ran though on does not list commit: {}",
        records(&home)
    );
}
