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

// setup-go has a post step, which act runs from the copy of the action it made for the main step
const CI: &str = "\
on:
  push:
    branches: [main]
jobs:
  go:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-go@b7ad1dad31e06c5925ef5d2fc7ad053ef454303e
        with:
          go-version-file: go.mod
          cache: false
      - run: go version
";

#[test]
#[ignore = "needs act, Docker and the internet"]
fn an_actions_post_step_runs_with_a_home_written_with_dots() {
    let tmp = tempfile::tempdir().unwrap();
    // A `..` in the home made act look for the post step's script where it never copied it
    let home = tmp.path().join("work/../home");
    // SAFETY: this is the only test in this binary
    unsafe { std::env::set_var("SHIPCHECK_HOME", &home) };
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
    fs::write(work.join("go.mod"), "module x\n\ngo 1.22\n").unwrap();
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"ci.yml\"]\nnetwork = \"internet\"\nretries = 0\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);

    // The second run finds the action cached and runs act offline, which the bug needed too
    for n in 0..2 {
        fs::write(work.join("n.txt"), n.to_string()).unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", &n.to_string()]);
        let plan = run::plan(&work, "HEAD", "origin/main").unwrap();
        let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
        let log =
            fs::read_to_string(run::log_dir(&plan.key).join("ci.yml/go.log")).unwrap_or_default();
        assert!(entry.passed, "run {n}: {entry:?}\n{log}");
        assert!(
            log.contains("Success - Post actions/setup-go"),
            "run {n}:\n{log}"
        );
    }
}
