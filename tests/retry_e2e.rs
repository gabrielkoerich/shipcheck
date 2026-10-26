mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use shipcheck::run;

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

const CI: &str = "\
on:
  push:
    branches: [main]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - run: git diff --stat HEAD; git diff --quiet HEAD && echo shipcheck-ran-the-tests; exit 1
";

#[test]
#[ignore = "needs act and Docker"]
fn every_retry_sees_its_own_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary
    unsafe { std::env::set_var("SHIPCHECK_HOME", tmp.path().join("home")) };
    // Declared after `tmp`, so it drops first and removes the volumes while the home still exists
    let _volumes = common::CleanOnDrop;

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
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"ci.yml\"]\nretries = 2\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);

    // Each retry starts in the same slot right after the attempt before it fails, and each new
    // commit's run takes that slot right after the run before it ends
    let mut stale = Vec::new();

    for round in 0..3 {
        fs::write(work.join("round.txt"), round.to_string()).unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "round"]);
        let plan = run::plan(&work, "main", "origin/main").unwrap();
        let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
        assert!(!entry.passed);
        assert_eq!(entry.workflows[0].attempts, 3);
        let dir = run::log_dir(&plan.key).join("ci.yml");

        for name in ["test.attempt-1.log", "test.attempt-2.log", "test.log"] {
            let log = fs::read_to_string(dir.join(name)).unwrap();

            // Printed only when the job sees the commit's files, act echoes the command too
            if !log
                .lines()
                .any(|l| l.trim_end().ends_with("| shipcheck-ran-the-tests"))
            {
                stale.push(format!("run {round} {name}"));
            }
        }
    }

    assert!(
        stale.is_empty(),
        "attempts that never ran the job: {stale:?}"
    );
}
