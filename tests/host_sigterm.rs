#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) {
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
}

fn wait_for<T>(what: &str, within: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;

    loop {
        if let Some(v) = f() {
            return v;
        }

        assert!(Instant::now() < deadline, "no {what} within {within:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The one reap record a running check has: its coalition, its launchd job and its record file
fn record(home: &Path) -> Option<(u64, String, PathBuf)> {
    let entry = fs::read_dir(home.join("reap")).ok()?.flatten().next()?;
    let text = fs::read_to_string(entry.path()).ok()?;
    let mut parts = text.splitn(4, ' ');
    let coal = parts.next()?.parse().ok()?;
    let target = parts.nth(1)?.to_string();
    Some((coal, target, entry.path()))
}

#[test]
fn a_stopped_run_reaps_its_running_check_at_once() {
    if shipcheck::runner::host::in_a_jail() {
        // A jail of shipcheck's own tests has no launchd to reach
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("state");
    let origin = tmp.path().join("origin.git");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(
        tmp.path(),
        &["clone", "-q", &origin.to_string_lossy(), "work"],
    );
    let work = tmp.path().join("work");
    git(&work, &["checkout", "-q", "-b", "main"]);
    // The check's processes leave its group and would run for ten minutes
    let config = "runner = \"host\"\nretries = 0\n[host]\nchecks = { long = '''sleep 600 & set -m; ( exec sleep 601 ) & wait''' }\n";
    fs::write(work.join("shipcheck.toml"), config).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("more.txt"), "x").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "feature"]);

    let mut run = Command::new(env!("CARGO_BIN_EXE_shipcheck"))
        .args(["run", "--no-fetch"])
        .current_dir(&work)
        .env("SHIPCHECK_HOME", &home)
        .spawn()
        .unwrap();
    let (coal, target, file) = wait_for("reap record", Duration::from_secs(120), || record(&home));
    let members = wait_for("check processes", Duration::from_secs(30), || {
        let m = shipcheck::runner::host::coalition_members(coal);
        (m.len() >= 4).then_some(m)
    });

    // Safe: the run is this test's own child
    assert_eq!(
        unsafe { libc::kill(run.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let status = wait_for("run exit", Duration::from_secs(10), || {
        run.try_wait().unwrap()
    });
    assert!(!status.success());
    wait_for("empty coalition", Duration::from_secs(5), || {
        shipcheck::runner::host::coalition_members(coal)
            .is_empty()
            .then_some(())
    });
    assert!(!file.exists(), "the record stayed");
    let loaded = Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()
        .unwrap();
    assert!(!loaded.status.success(), "{target} stayed loaded");

    for pid in members {
        // Safe: signal 0 only checks that the process exists
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "{pid} outlived the run");
    }
}
