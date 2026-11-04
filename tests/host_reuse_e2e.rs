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
keep_passed = true

[host]
checks = { ok = "true", flaky = "test -n \"$FLAKY_PASS\"" }
env = ["FLAKY_PASS"]
"#;

#[test]
fn a_rerun_keeps_the_checks_that_passed_on_the_branch_last_failed_result() {
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

    let go = |work: &Path, failed: bool| {
        let plan = run::plan(work, "HEAD", "origin/main").unwrap();
        let mut lines = Vec::new();
        let mut say = |l: &str| lines.push(l.to_string());
        let entry = if failed {
            run::run_only_failed(&plan, &mut say)
        } else {
            run::run(&plan, &mut say)
        };
        eprintln!("{}", lines.join("\n"));
        (plan, entry.unwrap(), lines)
    };
    let run = |work: &Path| go(work, false);
    let commit = |work: &Path, body: &str| {
        fs::write(work.join("a.txt"), body).unwrap();
        git(work, &["add", "-A"]);
        git(work, &["commit", "-q", "-m", body]);
    };

    let (_, first, _) = run(&work);
    assert!(!first.passed, "{first:?}");
    assert_eq!(first.workflows[0].jobs["ok"], "success");
    assert_eq!(first.workflows[0].jobs["flaky"], "failure");

    // The same tree again keeps what passed on it and runs what failed
    let (_, again, lines) = run(&work);
    assert!(!again.passed);
    assert!(
        lines
            .iter()
            .any(|l| l == "host/ok: passed on this tree already, kept")
    );
    assert!(!lines.iter().any(|l| l.starts_with("host/ok: running")));
    assert!(lines.iter().any(|l| l.starts_with("host/flaky: running")));

    // A plain run of a new tree runs every check, even after a failure
    // SAFETY: no thread of this test runs while it changes the environment
    unsafe {
        std::env::set_var("FLAKY_PASS", "1");
    }
    commit(&work, "b");
    let (_, full, lines) = run(&work);
    assert!(full.passed, "{full:?}");
    assert!(lines.iter().any(|l| l.starts_with("host/ok: running")));
    assert!(full.workflows[0].kept.is_empty());

    // --only-failed on the next tree keeps what passed on the branch's last failed result
    // SAFETY: no thread of this test runs while it changes the environment
    unsafe {
        std::env::remove_var("FLAKY_PASS");
    }
    commit(&work, "b2");
    let (a2, failed_again, _) = run(&work);
    assert!(!failed_again.passed);
    // SAFETY: as above
    unsafe {
        std::env::set_var("FLAKY_PASS", "1");
    }
    commit(&work, "b3");
    let (b, second, lines) = go(&work, true);
    assert!(second.passed, "{second:?}");
    let kept = format!("host/ok: passed on tree {}, kept", &a2.tree[..12]);
    assert!(lines.contains(&kept), "{lines:?}");
    assert!(!lines.iter().any(|l| l.starts_with("host/ok: running")));
    assert!(lines.iter().any(|l| l.starts_with("host/flaky: running")));
    assert_eq!(second.workflows[0].kept["ok"], a2.tree);
    assert!(run::lookup(&b).is_some_and(|e| e.passed));

    // After a pass, --only-failed on a new tree keeps nothing and runs every check
    commit(&work, "c");
    let (_, third, lines) = go(&work, true);
    assert!(third.passed);
    assert!(lines.iter().any(|l| l.starts_with("host/ok: running")));
    assert!(third.workflows[0].kept.is_empty());
}
