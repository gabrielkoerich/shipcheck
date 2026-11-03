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

[host]
checks = { ok = "true", flaky = "test -n \"$FLAKY_PASS\"" }
env = ["FLAKY_PASS"]
"#;

#[test]
fn a_rerun_of_a_failed_tree_keeps_the_checks_that_passed() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary, and it sets the environment before any thread
    unsafe {
        std::env::set_var("SHIPCHECK_HOME", tmp.path().join(".local/state/shipcheck"));
        std::env::remove_var("FLAKY_PASS");
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
    git(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("a.txt"), "a").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "feature"]);

    let plan = run::plan(&work, "feature", "origin/main").unwrap();
    let first = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    assert!(!first.passed, "{first:?}");
    assert_eq!(first.workflows[0].jobs["ok"], "success");
    assert_eq!(first.workflows[0].jobs["flaky"], "failure");

    // SAFETY: no thread of this test runs while it changes the environment
    unsafe {
        std::env::set_var("FLAKY_PASS", "1");
    }

    let plan = run::plan(&work, "feature", "origin/main").unwrap();
    let mut lines = Vec::new();
    let second = run::run(&plan, &mut |l| lines.push(l.to_string())).unwrap();
    eprintln!("{}", lines.join("\n"));
    assert!(second.passed, "{second:?}");
    assert!(
        lines
            .iter()
            .any(|l| l == "host/ok: passed on this tree already, kept")
    );
    assert!(!lines.iter().any(|l| l.starts_with("host/ok: running")));
    assert!(lines.iter().any(|l| l.starts_with("host/flaky: running")));
    assert!(run::lookup(&plan).is_some_and(|e| e.passed));
}
