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

// On a PR, GitHub runs each of these steps, and each fails
const PR: &str = "\
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - run: echo setup
      - name: tests
        if: github.event_name == 'pull_request'
        run: exit 1
";

const VARS: &str = "\
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - if: vars.RUN_TESTS != 'false' && github.event_name != 'workflow_dispatch'
        run: exit 1
";

const ALWAYS: &str = "\
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - if: always() && github.event_name == 'pull_request'
        run: exit 1
";

// The step reads the event through the job's env
const ENV: &str = "\
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    env:
      IS_PR: ${{ github.event_name == 'pull_request' }}
    steps:
      - if: env.IS_PR == 'true'
        run: exit 1
";

// GitHub skips the step on a static env, the deploy on every PR and the report on a passing
// job, and the last steps read the output of the first, past the guards
const OK: &str = "\
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        n: [1, 2]
    env:
      X: 'false'
    steps:
      - if: env.X == 'true'
        run: exit 1
      - id: o
        run: echo x=1 >> \"$GITHUB_OUTPUT\"
      - if: github.ref == 'refs/heads/main'
        run: exit 1
      - if: failure()
        run: exit 1
      - if: github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch'
        run: test \"${{ steps.o.outputs.x }}\" = 1
      - run: test \"${{ steps.o.outputs.x }}\" = 1
";

#[test]
#[ignore = "needs act and Docker"]
fn a_step_act_skips_fails_the_job_when_a_pr_may_run_it() {
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

    for (file, text) in [
        ("pr.yml", PR),
        ("vars.yml", VARS),
        ("always.yml", ALWAYS),
        ("env.yml", ENV),
        ("ok.yml", OK),
    ] {
        fs::write(work.join(".github/workflows").join(file), text).unwrap();
    }

    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"pr.yml\", \"vars.yml\", \"always.yml\", \"env.yml\", \"ok.yml\"]\nretries = 0\n[act]\ncache_paths = []\n",
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

    for file in ["pr.yml", "vars.yml", "always.yml", "env.yml"] {
        assert_eq!(state(file, "test"), "failure", "{file}");
    }

    assert_eq!(state("ok.yml", "test__1"), "success");
    assert_eq!(state("ok.yml", "test__2"), "success");
    assert!(!entry.passed, "{entry:?}");
    assert!(
        !run::cached(&plan.key).is_some_and(|e| e.passed),
        "a skip GitHub would run must never be stored as a pass"
    );
    let log = fs::read_to_string(run::log_dir(&plan.key).join("pr.yml/test.log")).unwrap();
    assert!(
        log.contains("act skipped the step 'tests' by its if:"),
        "{log}"
    );
}
