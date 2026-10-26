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
      - run: python3 -m venv /tmp/venv && /tmp/venv/bin/pip install -q pytest && echo /tmp/venv/bin >> \"$GITHUB_PATH\"
      - run: python -m pytest -q
";

/// A branch off main with these test files, its plan and its result
fn run_branch(work: &Path, name: &str, files: &[(&str, &str)]) -> (run::Plan, run::Entry) {
    git(work, &["checkout", "-q", "-b", name, "main"]);

    for (path, body) in files {
        fs::create_dir_all(work.join(path).parent().unwrap()).unwrap();
        fs::write(work.join(path), body).unwrap();
    }

    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", name]);
    let plan = run::plan(work, name, "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    eprintln!(
        "{}",
        fs::read_to_string(run::log_dir(&plan.key).join("ci.yml.log")).unwrap_or_default()
    );
    (plan, entry)
}

#[test]
#[ignore = "needs act, Docker and the internet for pip"]
fn shards_pytest_and_falls_back_to_one_run() {
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
        "workflows = [\"ci.yml\"]\nnetwork = \"internet\"\nshard_tests = 2\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);

    let (_, good) = run_branch(
        &work,
        "good",
        &[
            (
                "tests/test_a.py",
                "def test_a1(): pass\ndef test_a2(): pass\n",
            ),
            ("tests/test_b.py", "def test_b(): pass\n"),
        ],
    );
    assert!(good.passed, "{good:?}");
    let info = &good.workflows[0].shards["test"];
    assert_eq!(
        (info.shards, info.items, &info.fallback),
        (Some(2), Some(3), &None)
    );
    assert_eq!(good.workflows[0].attempts, 1);

    // test_b passes only after test_a ran in the same process, so it fails sharded and passes whole
    let (_, order) = run_branch(
        &work,
        "order",
        &[
            ("tests/state.py", "seen = []\n"),
            (
                "tests/test_a.py",
                "from tests import state\ndef test_a(): state.seen.append(1)\n",
            ),
            (
                "tests/test_b.py",
                "from tests import state\ndef test_b(): assert state.seen\n",
            ),
            ("tests/__init__.py", ""),
        ],
    );
    assert!(order.passed, "{order:?}");
    let info = &order.workflows[0].shards["test"];
    assert_eq!(info.fallback.as_deref(), Some("the job failed"));
    // Docker Desktop can show an attempt right after another the old nonce, which costs a retry
    assert!(order.workflows[0].attempts >= 2);

    let (_, failing) = run_branch(
        &work,
        "failing",
        &[
            ("tests/test_a.py", "def test_a(): pass\n"),
            ("tests/test_b.py", "def test_b(): assert False\n"),
        ],
    );
    assert!(!failing.passed, "{failing:?}");
    assert_eq!(failing.workflows[0].jobs["test"], "failure");
    assert!(failing.workflows[0].shards["test"].fallback.is_some());
    // One sharded attempt, then the unsharded run and its retry
    assert_eq!(failing.workflows[0].attempts, 3);
}
