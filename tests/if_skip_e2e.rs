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

// On a PR, GitHub runs both jobs, and both fail
const DISPATCH: &str = "\
on:
  pull_request:
  workflow_dispatch:
jobs:
  test:
    if: github.event_name != 'workflow_dispatch'
    runs-on: ubuntu-latest
    strategy:
      matrix:
        n: [1, 2]
    steps:
      - run: exit 1
";

const VARS: &str = "\
on: [push, pull_request]
jobs:
  e2e:
    if: vars.RUN_E2E == 'true'
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
";

// GitHub skips `deploy` on every PR, and the chain that needs it, so the skips stand beside a
// job that passed
const PUSH: &str = "\
on: [push, pull_request]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - run: true
  deploy:
    if: github.event_name == 'push'
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
  publish:
    needs: deploy
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
  notify:
    needs: [publish]
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
";

// GitHub runs `report` on a PR although its need is skipped
const ALWAYS: &str = "\
on: [push, pull_request]
jobs:
  deploy:
    if: github.event_name == 'push'
    runs-on: ubuntu-latest
    steps:
      - run: true
  report:
    needs: deploy
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
";

#[test]
#[ignore = "needs act and Docker"]
fn a_skip_passes_only_when_github_skips_the_job_on_every_pr() {
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
    fs::write(work.join(".github/workflows/dispatch.yml"), DISPATCH).unwrap();
    fs::write(work.join(".github/workflows/vars.yml"), VARS).unwrap();
    fs::write(work.join(".github/workflows/push.yml"), PUSH).unwrap();
    fs::write(work.join(".github/workflows/always.yml"), ALWAYS).unwrap();
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"dispatch.yml\", \"vars.yml\", \"push.yml\", \"always.yml\"]\nretries = 0\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("f"), "x").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "feature"]);

    let plan = run::plan(&work, "feature", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    let state = |file: &str, job: &str| {
        entry
            .workflows
            .iter()
            .find(|w| w.file == file)
            .unwrap_or_else(|| panic!("{file}: {entry:?}"))
            .jobs[job]
            .clone()
    };
    let skip_fails = |s: String| assert!(s.starts_with("failure, act skipped it"), "{s}");
    skip_fails(state("dispatch.yml", "test__1"));
    skip_fails(state("dispatch.yml", "test__2"));
    skip_fails(state("vars.yml", "e2e"));
    assert_eq!(state("push.yml", "check"), "success");
    for job in ["deploy", "publish", "notify"] {
        assert_eq!(state("push.yml", job), "skipped", "{job}");
    }
    assert!(
        state("always.yml", "report").starts_with("failure, a job it needs was skipped"),
        "{entry:?}"
    );
    assert!(!entry.passed, "{entry:?}");
    assert!(
        !run::cached(&plan.key).is_some_and(|e| e.passed),
        "a skip GitHub would run must never be stored as a pass"
    );

    // A partial run takes the same path, and the legit skip still passes
    let only = run::run_partial(&plan, &["e2e".into()], &[], &mut |l| eprintln!("{l}")).unwrap();
    assert!(!only.passed, "{only:?}");
    let only =
        run::run_partial(&plan, &[], &["push.yml".into()], &mut |l| eprintln!("{l}")).unwrap();
    assert!(only.passed, "{only:?}");
}
