use std::fs;
use std::path::Path;
use std::process::Command;

use shipcheck::{run, setup};

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

/// Removes only the volumes of this test's home, never runs `clean` against the real Docker
struct OwnVolumes(String);

impl Drop for OwnVolumes {
    fn drop(&mut self) {
        let out = Command::new("docker")
            .args(["volume", "ls", "-q", "--filter"])
            .arg(format!("name={}", self.0))
            .output()
            .unwrap();
        let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|n| n.starts_with(&self.0))
            .map(str::to_string)
            .collect();

        if !names.is_empty() {
            let _ = Command::new("docker")
                .args(["volume", "rm"])
                .args(&names)
                .output();
        }
    }
}

// GitHub accepts a top-level `cache-mode`, act 0.2.89's schema does not
const CI: &str = "\
on: [push, pull_request]
cache-mode: none
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: make lint
      - uses: some/action@v1
";

#[test]
#[ignore = "needs act and Docker"]
fn a_workflow_act_cannot_parse_stays_on_github_and_fails_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary
    unsafe { std::env::set_var("SHIPCHECK_HOME", tmp.path().join("home")) };
    let _volumes = OwnVolumes(shipcheck::runner::act::home_prefix());

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
    let machine = setup::Machine {
        docker: true,
        ..Default::default()
    };

    // The same job without `cache-mode` goes to act, so act parsing it is what keeps it
    let valid = CI.replace("cache-mode: none\n", "");
    fs::write(work.join(".github/workflows/ci.yml"), &valid).unwrap();
    let found = setup::detect_on(&work, &machine);
    assert!(
        found.acts.iter().any(|(k, _)| k.job == "lint"),
        "{}",
        found.explain()
    );

    fs::write(work.join(".github/workflows/ci.yml"), CI).unwrap();
    let found = setup::detect_on(&work, &machine);
    assert!(found.acts.is_empty(), "{}", found.explain());
    assert!(
        found.explain().contains(
            "act cannot run it either: act cannot parse its workflow: workflow is not valid. 'ci.yml': Line: 2 Column 1: Unknown Property cache-mode"
        ),
        "{}",
        found.explain()
    );

    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"ci.yml\"]\nretries = 2\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);

    // act refuses it before any container, every attempt the same way
    let plan = run::plan(&work, "main", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    assert!(!entry.passed);
    assert_eq!(entry.workflows[0].attempts, 1);
}
