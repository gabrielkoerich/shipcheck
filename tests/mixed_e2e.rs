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
  build:
    runs-on: ubuntu-latest
    steps:
      - run: test -f marker.txt
  e2e:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: test -f marker.txt
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
";

const CONFIG: &str = "\
runner = \"host\"

[host.checks]
test = \"test -f marker.txt\"

[act]
cache_paths = []

[jobs.\"ci.yml/e2e\"]
runner = \"act\"
";

#[test]
#[ignore = "needs act and Docker"]
fn host_checks_and_an_act_job_run_as_one_result() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary
    unsafe { std::env::set_var("SHIPCHECK_HOME", tmp.path().join("home")) };

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
    git(&work, &["fetch", "-q", "origin"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("marker.txt"), "ok").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "feature"]);

    let plan = run::plan(&work, "feature", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    let states: Vec<(String, Vec<(String, String)>)> = entry
        .workflows
        .iter()
        .map(|w| (w.file.clone(), w.jobs.clone().into_iter().collect()))
        .collect();
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        states,
        [
            ("host".to_string(), vec![pair("test", "success")]),
            (
                "ci.yml".to_string(),
                vec![pair("build", "success"), pair("e2e", "success")]
            ),
        ],
        "lint is not configured, so it never runs"
    );
    assert!(entry.passed);
    assert!(run::cached(&plan.key).unwrap().passed);

    // A partial run picks a host check or an act job by name
    let only = run::run_partial(&plan, &["e2e".into()], &[], &mut |l| eprintln!("{l}")).unwrap();
    assert_eq!(only.workflows.len(), 1);
    assert_eq!(
        only.workflows[0].jobs.len(),
        2,
        "e2e and the build it needs"
    );
    let only = run::run_partial(&plan, &["test".into()], &[], &mut |l| eprintln!("{l}")).unwrap();
    assert_eq!(only.workflows.len(), 1);
    assert_eq!(only.workflows[0].file, "host");
    assert!(run::run_partial(&plan, &["lint".into()], &[], &mut |_| {}).is_err());

    // The act job fails on its own when the tree lacks what it needs, beside a host check that fails too
    git(&work, &["rm", "-q", "marker.txt"]);
    git(&work, &["commit", "-q", "-m", "no marker"]);
    let plan = run::plan(&work, "feature", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    assert!(!entry.passed);
    assert_eq!(entry.workflows[1].jobs["build"], "failure");
    assert_eq!(
        entry.workflows[1].jobs["e2e"],
        "not run, a job it needs did not pass"
    );
    assert_eq!(entry.workflows[0].jobs["test"], "failure");
}
