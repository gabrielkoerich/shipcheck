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

const CONFIG: &str = r#"runner = "host"
retries = 0
shard_tests = 2

[host]
checks = { test = "test -f tests/test_a.py && uv run --no-project --with pytest pytest -q" }
"#;

const TESTS: [(&str, &str); 3] = [
    (
        "tests/test_a.py",
        "def test_one():\n    pass\n\ndef test_two():\n    pass\n",
    ),
    ("tests/test_b.py", "def test_three():\n    pass\n"),
    (
        "tests/test_c.py",
        "def test_four():\n    pass\n\ndef test_five():\n    pass\n",
    ),
];

/// A branch off main with these files, its plan and its result
fn run_branch(work: &Path, name: &str, files: &[(&str, &str)]) -> run::Entry {
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
        fs::read_to_string(run::log_dir(&plan.key).join("host.log")).unwrap_or_default()
    );
    entry
}

#[test]
fn shards_a_host_pytest_command_and_falls_back_to_one_run() {
    if Command::new("uv").arg("--version").output().is_err() {
        eprintln!("skipped: no uv on PATH");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary, and it sets the environment before any thread
    unsafe {
        std::env::set_var("SHIPCHECK_HOME", tmp.path().join(".local/state/shipcheck"));
    }

    let origin = tmp.path().join("origin.git");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(
        tmp.path(),
        &["clone", "-q", &origin.to_string_lossy(), "work"],
    );
    let work = tmp.path().join("work");
    git(&work, &["checkout", "-q", "-b", "main"]);
    fs::write(work.join("shipcheck.toml"), CONFIG).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);

    let good = run_branch(&work, "good", &TESTS);
    assert!(good.passed, "{good:?}");
    let info = &good.workflows[0].shards["test"];
    assert_eq!(
        (info.shards, info.items, &info.fallback),
        (Some(2), Some(5), &None)
    );

    let mut failing = TESTS.to_vec();
    failing.push(("tests/test_d.py", "def test_six():\n    assert False\n"));
    let bad = run_branch(&work, "bad", &failing);
    assert!(!bad.passed, "{bad:?}");
    let info = &bad.workflows[0].shards["test"];
    assert_eq!(info.shards, None);
    assert!(info.fallback.is_some(), "{info:?}");
    assert_eq!(bad.workflows[0].jobs["test"], "failure");
}
