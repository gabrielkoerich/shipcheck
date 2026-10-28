mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use sha2::Digest;
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
            // Setup commits start no background run, the steps that test the hooks call git themselves
            "-c",
            "core.hooksPath=/dev/null",
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
  check:
    runs-on: ubuntu-latest
    steps:
      - run: test -f marker.txt
";

const DEPLOY: &str = "\
on:
  push:
    branches: [main]
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
";

#[test]
#[ignore = "needs act and Docker"]
fn runs_caches_and_tests_the_working_copy() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary that reads or writes the environment
    unsafe { std::env::set_var("SHIPCHECK_HOME", tmp.path().join("home")) };
    // A hook finds shipcheck on PATH, which must be this build and never the installed one
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_shipcheck")).parent().unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());
    unsafe { std::env::set_var("PATH", &path) };
    // Declared after `tmp`, so it drops first and removes the volumes while the home still exists
    let _volumes = common::CleanOnDrop::new(&tmp.path().join("home"));

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
    fs::write(work.join(".github/workflows/deploy.yml"), DEPLOY).unwrap();
    // No cache volumes, they would be named after this temp repo and pile up across test runs
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"ci.yml\"]\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);

    git(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("marker.txt"), "ok").unwrap();
    // act would fail on this flag if it read the commit's own .actrc
    fs::write(work.join(".actrc"), "--no-such-flag\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "feature"]);

    let plan = run::plan(&work, "feature", "origin/main").unwrap();
    let feature_key = plan.key.clone();
    assert!(run::cached(&plan.key).is_none());
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    assert!(
        entry.passed,
        "{entry:?}\n{}",
        fs::read_to_string(run::log_dir(&plan.key).join("ci.yml.log")).unwrap_or_default()
    );
    assert_eq!(entry.workflows[0].jobs["check"], "success");
    assert!(run::cached(&plan.key).unwrap().passed);

    // A named commit is tested as it is, its working copy does not count
    git(&work, &["checkout", "-q", "main"]);
    fs::write(work.join("marker.txt"), "uncommitted").unwrap();
    let plan = run::plan(&work, "main", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    assert!(
        !entry.passed,
        "an uncommitted marker must not count for a named commit: {entry:?}"
    );
    assert_eq!(
        entry.workflows[0].jobs["check"],
        "failure",
        "{}",
        fs::read_to_string(run::log_dir(&plan.key).join("ci.yml.log")).unwrap_or_default()
    );
    assert_eq!(entry.workflows[0].attempts, 2);

    let bin = Path::new(env!("CARGO_BIN_EXE_shipcheck"));
    let path = format!(
        "{}:{}",
        bin.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let push = |branch: &str| {
        Command::new("git")
            .args(["push", "-q", "origin", branch])
            .env("PATH", &path)
            .current_dir(&work)
            .status()
            .unwrap()
            .success()
    };

    fs::remove_file(work.join("marker.txt")).unwrap();
    git(&work, &["checkout", "-q", "feature"]);
    let status = Command::new(bin)
        .args(["status", "feature"])
        .env("PATH", &path)
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "the CLI must see the cached pass for key {}: {}{}",
        feature_key,
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let init = Command::new(bin)
        .args(["init", "--yes"])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(work.join(".git/hooks/pre-push").exists());
    assert!(
        !work.join(".githooks").exists(),
        "nothing in the working tree may hold the hook"
    );
    assert!(
        push("feature"),
        "the hook must let a passing commit through"
    );

    git(&work, &["checkout", "-q", "-b", "broken"]);
    git(&work, &["rm", "-q", "marker.txt"]);
    git(&work, &["commit", "-q", "-m", "broken"]);
    assert!(push("broken"), "the hook must never block a push");

    // The push started a background run, whose failure turns the check red
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);

    let verdict = loop {
        let code = Command::new(bin)
            .args(["status", "broken"])
            .env("PATH", &path)
            .current_dir(&work)
            .output()
            .unwrap()
            .status
            .code();

        if code != Some(2) {
            break code;
        }

        assert!(
            std::time::Instant::now() < deadline,
            "the push must start a run"
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    assert_eq!(
        verdict,
        Some(1),
        "the broken commit must fail in the background"
    );

    // A branch that rewrites its own workflow to pass still runs main's workflow, and fails
    git(&work, &["checkout", "-q", "-b", "cheat", "broken"]);
    fs::write(
        work.join(".github/workflows/ci.yml"),
        CI.replace("test -f marker.txt", "true"),
    )
    .unwrap();
    git(&work, &["commit", "-qam", "make ci pass"]);
    let cheat = Command::new(bin)
        .args(["run", "--no-fetch", "cheat"])
        .env("PATH", &path)
        .current_dir(&work)
        .output()
        .unwrap();
    assert_eq!(
        cheat.status.code(),
        Some(1),
        "a branch must not pass by editing its own workflow"
    );

    let left = Command::new("docker")
        .args(["ps", "-aq", "--filter", "name=workflows-ci-yml-"])
        .output()
        .unwrap();
    assert!(
        left.stdout.is_empty(),
        "a failed run must not leave its job container behind"
    );

    git(&work, &["checkout", "-q", "-b", "twice", "feature"]);
    fs::write(work.join("marker.txt"), "again").unwrap();
    git(&work, &["commit", "-qam", "twice"]);
    let spawn = || {
        Command::new(bin)
            .args(["run", "--no-fetch"])
            .env("PATH", &path)
            .current_dir(&work)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (spawn(), spawn());
    let outs = [a.wait_with_output().unwrap(), b.wait_with_output().unwrap()];
    assert!(
        outs.iter().all(|o| o.status.success()),
        "{:?}",
        outs.iter()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .collect::<Vec<_>>()
    );
    let ran = outs
        .iter()
        .filter(|o| String::from_utf8_lossy(&o.stdout).contains("running, attempt"))
        .count();
    assert_eq!(ran, 1, "two requests for one tree must share one run");

    // The owner adds commit to on, then a plain commit starts a run in the background
    git(&work, &["checkout", "-q", "main"]);
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"ci.yml\"]\non = [\"push\", \"commit\"]\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["commit", "-qam", "run on commit"]);
    git(&work, &["push", "-q", "--no-verify", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    let install = Command::new(bin)
        .arg("install")
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(install.status.success());
    git(&work, &["checkout", "-q", "-b", "oncommit", "main"]);
    fs::write(work.join("marker.txt"), "from a commit").unwrap();
    git(&work, &["add", "-A"]);
    let commit = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(["commit", "-q", "-m", "oncommit"])
        .env("PATH", &path)
        .current_dir(&work)
        .status()
        .unwrap();
    assert!(commit.success(), "the commit hook must never fail a commit");

    let status = || {
        Command::new(bin)
            .args(["status", "--no-fetch"])
            .env("PATH", &path)
            .current_dir(&work)
            .output()
            .unwrap()
            .status
            .success()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);

    while !status() {
        assert!(
            std::time::Instant::now() < deadline,
            "the commit hook must start a run that caches a result. Its log:\n{}",
            fs::read_to_string(tmp.path().join("home/logs/background.log")).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // A newer commit on the same branch cancels the run in progress, like cancel-in-progress
    git(&work, &["checkout", "-q", "main"]);
    fs::write(work.join(".github/workflows/ci.yml"), SLOW_CI).unwrap();
    git(&work, &["commit", "-qam", "slow when asked"]);
    git(&work, &["push", "-q", "--no-verify", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    git(&work, &["checkout", "-q", "-b", "cancel", "main"]);
    let commit = |message: &str| {
        git(&work, &["add", "-A"]);
        let ok = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(["commit", "-q", "-m", message])
            .env("PATH", &path)
            .current_dir(&work)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        git(&work, &["rev-parse", "HEAD"])
    };
    let code_of = |sha: &str| {
        Command::new(bin)
            .args(["status", sha])
            .env("PATH", &path)
            .current_dir(&work)
            .output()
            .unwrap()
            .status
            .code()
    };

    let background_log =
        || fs::read_to_string(tmp.path().join("home/logs/background.log")).unwrap_or_default();
    fs::write(work.join("marker.txt"), "slow").unwrap();
    fs::write(work.join("slow"), "").unwrap();
    let slow = commit("slow");
    std::thread::sleep(std::time::Duration::from_secs(3));
    fs::remove_file(work.join("slow")).unwrap();
    let fast = commit("fast");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);

    while code_of(&fast) != Some(0) {
        assert!(
            std::time::Instant::now() < deadline,
            "the newer commit must pass: {}",
            background_log()
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    assert_eq!(
        code_of(&slow),
        Some(2),
        "the cancelled run must leave no result: {}",
        background_log()
    );
    let left = Command::new("docker")
        .args(["ps", "-a", "--filter", "name=workflows-ci-yml-"])
        .args(["--format", "{{.Names}} {{.Status}} {{.Mounts}}"])
        .output()
        .unwrap();
    assert!(
        left.stdout.is_empty(),
        "a cancelled run must not leave its job container behind: {}{}",
        String::from_utf8_lossy(&left.stdout),
        background_log()
    );

    // The agent's run before a commit is the check: committing that tree runs nothing again
    git(&work, &["checkout", "-q", "-b", "wip", "main"]);
    // `shipcheck commit` runs plain `git commit`, which needs an identity on a CI runner
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["config", "user.email", "t@t"]);
    git(&work, &["config", "commit.gpgsign", "false"]);
    fs::write(work.join("marker.txt"), "tested before the commit").unwrap();
    let shipcheck = |args: &[&str]| {
        let out = Command::new(bin)
            .args(args)
            .env("PATH", &path)
            .current_dir(&work)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    };
    let worktree = || git(&work, &["status", "--porcelain", "--ignored"]);
    let before = worktree();
    let (code, out) = shipcheck(&["run", "--no-fetch"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("tested the working copy, tree "), "{out}");
    assert!(
        out.contains("1 untracked file is part of this run: marker.txt"),
        "{out}"
    );
    assert_eq!(
        worktree(),
        before,
        "the run must not write into the agent's worktree"
    );

    fs::write(work.join("late.txt"), "after the run").unwrap();
    let (code, out) = shipcheck(&["commit", "-m", "late"]);
    assert_ne!(code, Some(0), "{out}");
    assert!(out.contains("files changed since the run"), "{out}");
    fs::remove_file(work.join("late.txt")).unwrap();

    let (code, out) = shipcheck(&["commit", "-m", "wip"]);
    assert_eq!(code, Some(0), "{out}");
    let wip = git(&work, &["rev-parse", "HEAD"]);
    assert_eq!(
        code_of(&wip),
        Some(0),
        "the commit must find the run's result at once"
    );
    let record = |branch: &str| {
        tmp.path()
            .join("home/background")
            .join(run::repo_id(&work).unwrap())
            .join(format!("{branch}.pid"))
    };
    assert!(!record("wip").exists(), "the commit hook must start no run");

    // With --staged, the index is what ran, and a plain `git commit` of it reuses the result
    git(&work, &["checkout", "-q", "-b", "staged", "main"]);
    fs::write(work.join("marker.txt"), "staged").unwrap();
    git(&work, &["add", "marker.txt"]);
    fs::write(work.join("not-staged.txt"), "left out").unwrap();
    let (code, out) = shipcheck(&["run", "--no-fetch", "--staged"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("tested the index, tree "), "{out}");
    let ok = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(["commit", "-q", "-m", "staged"])
        .env("PATH", &path)
        .current_dir(&work)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    assert_eq!(
        code_of("HEAD"),
        Some(0),
        "the index's commit must hit the cache"
    );
    assert!(!record("staged").exists());

    // Another tree, the file left out of the index, has no result
    git(&work, &["add", "not-staged.txt"]);
    git(
        &work,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "-m",
            "more",
        ],
    );
    assert_eq!(code_of("HEAD"), Some(2), "a different tree must run again");

    // With paths set on main, a change outside them passes without running
    git(&work, &["checkout", "-q", "main"]);
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"ci.yml\"]\npaths = [\"src/**\"]\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["commit", "-qam", "only src counts"]);
    git(&work, &["push", "-q", "--no-verify", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    git(&work, &["checkout", "-q", "-b", "docs-only", "main"]);
    fs::write(work.join("README.md"), "docs").unwrap();
    let docs = commit("docs only");
    let out = Command::new(bin)
        .args(["status", &docs])
        .env("PATH", &path)
        .current_dir(&work)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a change outside paths must pass"
    );

    // Each job has its own checkout, gets the outputs of its needs, skips with a skipped need, and
    // fails without failing the workflow under continue-on-error
    git(&work, &["checkout", "-q", "main"]);
    fs::write(work.join(".github/workflows/jobs.yml"), JOBS).unwrap();
    // A second workflow runs beside the first, still within max_parallel jobs
    fs::write(
        work.join(".github/workflows/second.yml"),
        "on: push\njobs:\n  only:\n    runs-on: ubuntu-latest\n    steps: [{run: echo second}]\n",
    )
    .unwrap();
    fs::create_dir_all(work.join(".github/workflows/scripts")).unwrap();
    fs::write(
        work.join(".github/workflows/scripts/hello.sh"),
        "echo hello\n",
    )
    .unwrap();
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"jobs.yml\", \"second.yml\"]\nmax_parallel = 1\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "jobs"]);
    git(&work, &["push", "-q", "--no-verify", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    let plan = run::plan(&work, "main", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    let jobs = &entry.workflows[0].jobs;
    assert!(entry.passed, "{entry:?}");
    assert_eq!(entry.workflows.len(), 2);
    assert_eq!(entry.workflows[1].file, "second.yml");
    assert_eq!(entry.workflows[1].jobs["only"], "success");
    assert_eq!(jobs["a"], "success");
    assert_eq!(jobs["b"], "success", "b saw a's file or not its output");
    assert_eq!(jobs["c"], "skipped");
    assert_eq!(jobs["d"], "skipped");
    assert_eq!(jobs["e"], "failure, allowed by continue-on-error");
    assert_eq!(
        jobs["f"], "success",
        "a job that needs an allowed failure still runs"
    );

    // A job reaches nothing of the host: no socket, token, environment, network or real .git
    git(&work, &["checkout", "-q", "main"]);
    git(
        &work,
        &[
            "config",
            "credential.probe",
            "https://u:host-secret@example.com",
        ],
    );
    let own_hash = shipcheck::cache::hex(&sha2::Sha256::digest(b"own-canary-5d1e"));
    // A listener on the host's loopback, and host files a job's checkout links to
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let host = tmp.path().join("host");
    fs::create_dir_all(&host).unwrap();
    fs::write(host.join("canary"), "mine").unwrap();
    fs::write(
        work.join(".github/workflows/isolation.yml"),
        ISOLATION
            .replace(
                "HOST_PORT",
                &listener.local_addr().unwrap().port().to_string(),
            )
            .replace("HOST_DIR", &host.to_string_lossy())
            .replace("OWN_HASH", &own_hash[..16]),
    )
    .unwrap();
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"isolation.yml\"]\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    // act reads these from its working directory, so a commit could set its own secrets or flags
    for (file, text) in [
        (".actrc", "--env ACTRC_CANARY=from-dotfile\n"),
        (".env", "DOTENV_CANARY=from-dotfile\n"),
        (".secrets", "DOT_SECRET=from-dotfile\n"),
        (".vars", "DOT_VAR=from-dotfile\n"),
        (".input", "dot_input=from-dotfile\n"),
    ] {
        fs::write(work.join(file), text).unwrap();
    }
    // The secrets file of this repository reaches its jobs, another repository's does not
    let id = run::repo_id(&work).unwrap();
    let own = run::secrets_file(&id);
    fs::create_dir_all(own.parent().unwrap()).unwrap();
    fs::write(&own, "OWN_SECRET=own-canary-5d1e\n").unwrap();
    let other = run::secrets_file(&format!("{}/other", id.split('/').next().unwrap()));
    fs::write(&other, "OTHER_SECRET=canary-of-another-repository\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "isolation"]);
    git(&work, &["push", "-q", "--no-verify", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    // An unpushed branch and a stash hold work the job must not see
    git(&work, &["checkout", "-q", "-b", "private-wip"]);
    fs::write(work.join("wip.txt"), "unpushed-wip").unwrap();
    git(&work, &["add", "wip.txt"]);
    git(&work, &["commit", "-qm", "unpushed-wip"]);
    git(&work, &["tag", "private-tag"]);
    git(&work, &["checkout", "-q", "main"]);
    git(&work, &["tag", "v-public"]);
    fs::write(work.join("stashed.txt"), "stashed-wip").unwrap();
    git(&work, &["stash", "push", "-q", "-u", "-m", "stashed-wip"]);
    // SAFETY: this is the only test in this binary that reads or writes the environment
    unsafe {
        std::env::set_var("GITHUB_TOKEN", "host-token");
        std::env::set_var("HOST_CANARY", "host-canary");
    }
    let plan = run::plan(&work, "main", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    unsafe {
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("HOST_CANARY");
    }
    assert!(
        entry.passed,
        "a job reached the host: {}",
        fs::read_to_string(run::log_dir(&plan.key).join("isolation.yml.log")).unwrap_or_default()
    );
    assert!(
        !work.join(".git/written-by-job").exists(),
        "a job wrote into the host's .git"
    );
    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "a job reached a listener on the host's loopback"
    );
    // The job left git config, hooks, filters, links and a process rewriting its .git, and nothing
    // shipcheck did afterwards may run them or touch what they point at
    let ran: Vec<_> = fs::read_dir(&host)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ran-"))
        .collect();
    assert!(ran.is_empty(), "the job's checkout ran {ran:?} on the host");
    assert_eq!(fs::read_to_string(host.join("canary")).unwrap(), "mine");
    let log =
        fs::read_to_string(run::log_dir(&plan.key).join("isolation.yml.log")).unwrap_or_default();
    assert!(log.contains("secret is ***"), "{log}");
    // An output that names a secret never makes act read it for the job that gets the output
    let used = fs::read_to_string(run::log_dir(&plan.key).join("isolation.yml/use.log")).unwrap();
    assert!(used.contains("shipcheck-end"), "{used}");
    assert!(
        !used.contains("own-canary") && !used.contains("***"),
        "an output became an expression: {used}"
    );
    // Log tails posted to the check never carry a value of the secrets file
    let mut failed = entry;
    failed.workflows[0].passed = false;
    let tail = shipcheck::check::failure_text(
        &failed,
        &run::log_dir(&plan.key),
        &run::secret_values(&id).unwrap(),
    );
    assert!(tail.contains("isolation.yml"), "{tail}");
    assert!(!log.contains("own-canary") && !tail.contains("own-canary"));
    // `.cargo/config.toml` keeps the secrets file a file, out of the login keychain
    assert!(own.exists());
    shipcheck::secret::remove(&own).unwrap();
    // `shipcheck logs` hands no control character from a job to the terminal
    let logs = Command::new(bin)
        .args(["logs", "main"])
        .env("PATH", &path)
        .current_dir(&work)
        .output()
        .unwrap();
    let printed = String::from_utf8_lossy(&logs.stdout);
    assert!(logs.status.success(), "{printed}");
    assert!(printed.contains("\\u{1b}]0;from-job"), "{printed}");
    assert!(!logs.stdout.contains(&0x1b) && !logs.stdout.contains(&0x07));
    fs::remove_file(&other).unwrap();

    // A branch's run leaves nothing in a later run of main but its ignored build directories and
    // the cache volumes, which carry over by design
    fs::write(work.join(".github/workflows/state.yml"), STATE).unwrap();
    fs::write(work.join(".gitignore"), "build\n.hidden\n").unwrap();
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"state.yml\"]\n[act]\ncache_paths = [\"/home/runner/.shipcheck\"]\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "state"]);
    git(&work, &["push", "-q", "--no-verify", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    git(&work, &["checkout", "-q", "-b", "plant"]);
    fs::write(work.join("plant"), "").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "plant"]);
    let state_log = |plan: &run::Plan| {
        fs::read_to_string(run::log_dir(&plan.key).join("state.yml.log")).unwrap_or_default()
    };
    let plan = run::plan(&work, "plant", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    assert!(entry.passed, "{}", state_log(&plan));
    git(&work, &["checkout", "-q", "main"]);
    let plan = run::plan(&work, "main", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    let log = state_log(&plan);
    assert!(entry.passed, "a branch's run left state in main's: {log}");
    assert!(log.contains("volume carried: yes"), "{log}");
    // Warming needs copy-on-write, which the temp dirs of Linux CI runners lack
    if cfg!(target_os = "macos") {
        assert!(log.contains("build carried: yes"), "{log}");
    }

    // A job past its timeout is killed with its container and every process, and what it kept
    // writing into its .git runs nothing on the host
    fs::write(
        work.join(".github/workflows/hang.yml"),
        HANG.replace("HOST_DIR", &host.to_string_lossy()),
    )
    .unwrap();
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"hang.yml\"]\ntimeout_minutes = 1\nretries = 0\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "hang"]);
    git(&work, &["push", "-q", "--no-verify", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    let plan = run::plan(&work, "main", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    assert!(!entry.passed);
    assert_eq!(entry.workflows[0].jobs["hang"], "timed out", "{entry:?}");
    let left = Command::new("docker")
        .args(["ps", "-aq", "--filter", "name=workflows-hang-yml-"])
        .output()
        .unwrap();
    assert!(left.stdout.is_empty(), "a timed-out job left its container");
    let home = tmp.path().join("home");
    let procs = Command::new("pgrep")
        .args(["-fl", &home.to_string_lossy()])
        .output()
        .unwrap();
    assert!(
        procs.stdout.is_empty(),
        "a timed-out job left processes: {}",
        String::from_utf8_lossy(&procs.stdout)
    );
    assert!(!host.join("ran-timeout").exists());

    // With the network on, jobs pass artifacts through act's server, and reach the internet but not
    // the host's loopback
    fs::write(
        work.join(".github/workflows/artifacts.yml"),
        ARTIFACTS.replace(
            "HOST_PORT",
            &listener.local_addr().unwrap().port().to_string(),
        ),
    )
    .unwrap();
    fs::write(
        work.join("shipcheck.toml"),
        "workflows = [\"artifacts.yml\"]\nnetwork = \"internet\"\n[act]\ncache_paths = []\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "artifacts"]);
    git(&work, &["push", "-q", "--no-verify", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    let plan = run::plan(&work, "main", "origin/main").unwrap();
    let done = std::sync::atomic::AtomicBool::new(false);
    let (entry, modes) = std::thread::scope(|s| {
        // act removes job containers when they end, so their network mode is read while they run
        let modes = s.spawn(|| {
            let mut modes = std::collections::BTreeSet::new();
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let ids = Command::new("docker")
                    .args(["ps", "-q", "--filter", "name=workflows-artifacts-yml-"])
                    .output()
                    .unwrap();
                for id in String::from_utf8_lossy(&ids.stdout).split_whitespace() {
                    let out = Command::new("docker")
                        .args(["inspect", "-f", "{{.HostConfig.NetworkMode}}", id])
                        .output()
                        .unwrap();
                    let mode = String::from_utf8_lossy(&out.stdout).trim().to_string();

                    if !mode.is_empty() {
                        modes.insert(mode);
                    }
                }

                std::thread::sleep(std::time::Duration::from_millis(300));
            }

            modes
        });
        let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        (entry, modes.join().unwrap())
    });
    let log =
        fs::read_to_string(run::log_dir(&plan.key).join("artifacts.yml.log")).unwrap_or_default();
    assert!(
        entry.passed,
        "artifacts or the internet probe failed: {log}"
    );
    assert!(!modes.is_empty(), "saw no job container");
    assert!(
        modes
            .iter()
            .all(|m| m.starts_with("shipcheck-") && m.contains("-net-")),
        "an internet job ran outside its own network: {modes:?}"
    );
    let nets = Command::new("docker")
        .args(["network", "ls", "--format", "{{.Name}}"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&nets.stdout);
    assert!(
        modes.iter().all(|m| !listed.lines().any(|l| l == m)),
        "a run left its network: {modes:?}"
    );
    // On Linux the server listens on the run network's gateway, on a Mac on loopback behind
    // host.docker.internal. Never on every interface or the LAN address
    let server = log
        .lines()
        .find_map(|l| l.split_once("Start server on http://"))
        .and_then(|(_, rest)| rest.split_once(':'))
        .and_then(|(addr, _)| addr.parse::<std::net::Ipv4Addr>().ok())
        .expect("no artifact server address in the log");
    if cfg!(target_os = "macos") {
        assert!(server.is_loopback(), "{server}");
    } else {
        assert!(
            server.is_private() && !server.is_loopback() && server.octets()[3] == 1,
            "the artifact server must listen on the run network's gateway: {server}"
        );
    }

    // With no shipcheck.toml on main there is nothing to run, so nothing runs and nothing posts
    let bare = tmp.path().join("bare.git");
    fs::create_dir(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "-b", "main"]);
    git(
        tmp.path(),
        &["clone", "-q", &bare.to_string_lossy(), "fresh"],
    );
    let fresh = tmp.path().join("fresh");
    git(&fresh, &["checkout", "-q", "-b", "main"]);
    fs::create_dir_all(fresh.join(".github/workflows")).unwrap();
    fs::write(
        fresh.join(".github/workflows/pr.yml"),
        "on: [push, pull_request]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{run: echo pr}]\n",
    )
    .unwrap();
    git(&fresh, &["add", "-A"]);
    git(&fresh, &["commit", "-qm", "no config"]);
    git(&fresh, &["push", "-q", "origin", "main"]);
    git(&fresh, &["fetch", "-q", "origin"]);
    let cli = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .env("PATH", &path)
            .current_dir(&fresh)
            .output()
            .unwrap()
    };
    let nothing = "origin/main has no shipcheck.toml, so there is nothing to run";
    let doctor = cli(&["doctor"]);
    let said = String::from_utf8_lossy(&doctor.stdout).to_string();
    assert_eq!(doctor.status.code(), Some(1), "{said}");
    assert!(said.contains(nothing), "{said}");
    let ran = cli(&["run", "--no-fetch"]);
    let ran_out = String::from_utf8_lossy(&ran.stderr).to_string();
    assert_eq!(ran.status.code(), Some(2), "{ran_out}");
    assert!(ran_out.contains(nothing), "{ran_out}");
    // Off the default branch the refusal is the missing config
    git(&fresh, &["switch", "-q", "-c", "feature"]);
    fs::write(fresh.join("note.txt"), "x").unwrap();
    git(&fresh, &["add", "-A"]);
    git(&fresh, &["commit", "-qm", "note"]);
    assert_eq!(cli(&["run", "--no-fetch"]).status.code(), Some(2));
    let post = cli(&["post", "--home-ok"]);
    assert_eq!(post.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&post.stderr).contains(nothing));

    // clean removes this repository's warm template and slots, and --all also the results. It never
    // reaches the real Docker, whose removal rules the fake engine of gc's tests covers
    let home = tmp.path().join("home");
    let templates = home.join("templates");
    assert!(templates.read_dir().unwrap().next().is_some());
    let clean = |extra: &[&str]| {
        let out = Command::new(bin)
            .arg("clean")
            .args(extra)
            .env("PATH", &path)
            .env("DOCKER_HOST", "unix:///nonexistent")
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    clean(&[]);
    // Templates sit at templates/<owner>/<repo>. Nothing of this repository, `origin`, may be left,
    // and the other repository of this test keeps its own
    let left: Vec<_> = fs::read_dir(&templates)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|owner| fs::read_dir(owner.path()).into_iter().flatten().flatten())
        .map(|e| e.file_name())
        .filter(|name| name.to_string_lossy().contains("origin"))
        .collect();
    assert!(
        left.is_empty(),
        "clean must remove the repository's template and git-cow's files: {left:?}"
    );
    assert!(
        !git(&work, &["worktree", "list"]).contains("templates"),
        "clean must leave git no stale template worktree"
    );
    assert!(home.join("cache").exists());
    clean(&["--all"]);
    assert!(
        !home.join("cache").exists(),
        "clean --all must remove the results"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("nothing to run: no change under paths"));
}

const JOBS: &str = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    outputs:
      word: ${{ steps.w.outputs.word }}
    steps:
      - id: w
        run: |
          touch from-a
          echo \"word=it's here\" >> \"$GITHUB_OUTPUT\"
      # The checkout matches the commit, scripts next to the workflows stay, each job has its own tool cache
      - run: git diff --exit-code && git status --porcelain | grep -v from-a | grep -c . | grep -qx 0
      - run: sh .github/workflows/scripts/hello.sh
      - run: test \"$RUNNER_TOOL_CACHE:$AGENT_TOOLSDIRECTORY\" = /home/runner/.shipcheck/a/toolcache:/home/runner/.shipcheck/a/toolcache
      # The job runs as GitHub's runner user, and can write every mount
      - run: test \"$(id -un):$(id -u):$HOME\" = runner:1001:/home/runner && sudo -n true
      - run: test \"$ImageOS:$RUNNER_TEMP\" = ubuntu24:/home/runner/work/_temp && touch \"$RUNNER_TEMP/t\"
      - run: test -w .venv
      - run: test \"${{ github.ref }}\" = refs/heads/shipcheck
  b:
    needs: a
    runs-on: ubuntu-latest
    steps:
      - run: test ! -e from-a
      - run: test \"${{ needs.a.outputs.word }}\" = \"it's here\"
  c:
    if: false
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
  d:
    needs: c
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
  e:
    continue-on-error: true
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
  f:
    needs: e
    runs-on: ubuntu-latest
    steps:
      - run: echo ran
";

const ISOLATION: &str = r#"
on: push
jobs:
  probe:
    runs-on: ubuntu-latest
    steps:
      - run: test ! -e /var/run/docker.sock
      - run: test -z "${{ secrets.GITHUB_TOKEN }}"
      - run: "! env | grep -q -e host- -e from-dotfile"
      - run: test -z "${{ secrets.DOT_SECRET }}${{ vars.DOT_VAR }}${{ inputs.dot_input }}"
      - run: test -z "${{ secrets.OTHER_SECRET }}"
      # The value itself would show in the log as part of this script, so compare its hash
      - run: |
          test "$(printf %s '${{ secrets.OWN_SECRET }}' | sha256sum | cut -c1-16)" = OWN_HASH
          echo "secret is ${{ secrets.OWN_SECRET }}"
      # No route anywhere: not the host's gateways, Docker's API ports, its proxy or this test's listener.
      # bash succeeds once it connects, so a listener that never answers still counts as reached
      - run: |
          test -z "$(awk '$2 == "00000000"' /proc/net/route)"
          for target in 127.0.0.1 host.docker.internal gateway.docker.internal 172.17.0.1 192.168.65.1 192.168.65.7 192.168.65.254; do
            for port in HOST_PORT 80 2375 2376 3128; do
              if timeout 3 bash -c "echo > /dev/tcp/$target/$port" 2>/dev/null; then echo "reached $target:$port"; exit 1; fi
            done
          done
      - run: "! getent hosts example.com && ! getent hosts host.docker.internal"
      - run: "! curl -s -m 5 -o /dev/null https://example.com"
      # A step container of its own gets no more than the job
      # The job's own image, since busybox has no connect-only probe
      - uses: docker://catthehacker/ubuntu@sha256:c58e2b364da03b0c804c7d660f2ecbedf2f221a382b9baa0b344b0144780ff43
        with:
          entrypoint: /bin/bash
          args: -c "test ! -e /var/run/docker.sock && ! timeout 3 bash -c 'echo > /dev/tcp/172.17.0.1/HOST_PORT' && ! timeout 3 bash -c 'echo > /dev/tcp/host.docker.internal/HOST_PORT'"
      - run: printf '\033]0;from-job\007\033[2J\n'
      - run: git rev-parse HEAD && git ls-files | grep -q isolation.yml
      - run: "! grep -rq host-secret .git"
      # Built at run time, since this file itself is one of the job's objects
      - run: "! git cat-file --batch-all-objects --batch | grep -aq -e unpushed-$(echo wip) -e stashed-$(echo wip)"
      - run: "! git rev-parse -q --verify private-wip && ! git rev-parse -q --verify refs/stash"
      - run: "! git rev-parse -q --verify private-tag && git describe --tags | grep -q v-public"
      - run: touch .git/written-by-job
      # What shipcheck must not run or follow once the job is done, and a process that keeps
      # rewriting .git for as long as the container lives
      - run: |
          git config core.fsmonitor "touch HOST_DIR/ran-fsmonitor"
          git config core.hooksPath HOST_DIR
          git config filter.x.clean "touch HOST_DIR/ran-clean"
          git config filter.x.process "touch HOST_DIR/ran-process"
          printf '* filter=x\n' > .gitattributes
          printf 'target\n' > .gitignore
          mkdir -p target
          ln -s HOST_DIR/canary target/canary
          ln -s HOST_DIR target/host
          ln .gitignore target/hard
          nohup sh -c 'while :; do rm -rf .git; mkdir -p .git; printf "[core]\n\tfsmonitor = touch HOST_DIR/ran-loop\n" > .git/config; sleep 0.2; done' > /dev/null 2>&1 &
  out:
    runs-on: ubuntu-latest
    outputs:
      v: ${{ steps.o.outputs.v }}
      q: ${{ steps.o.outputs.q }}
      b: ${{ steps.o.outputs.b }}
    steps:
      - id: o
        run: |
          d=$(printf '\044')
          { echo 'v<<EOF'; printf '%s\n' "x' }} ${d}{{ github.job }} ${d}{{ '" '- run: evil' 'k: [a, {b: "c' '#'; echo EOF; } >> "$GITHUB_OUTPUT"
          echo "q=x' }} ${d}{{ secrets.OWN_SECRET }} ${d}{{ 'y" >> "$GITHUB_OUTPUT"
          echo "b=x }} ${d}{{ secrets.OWN_SECRET }} y" >> "$GITHUB_OUTPUT"
  # An output with YAML and expression syntax arrives as the same string, never as workflow syntax,
  # whether a run step, an `if` or the job's env reads it
  use:
    needs: out
    if: needs.out.outputs.q != 'z' && needs.out.outputs.b != 'z'
    runs-on: ubuntu-latest
    env:
      GOT: ${{ needs.out.outputs.v }}
      Q: ${{ needs.out.outputs.q }}
      B: ${{ needs.out.outputs.b }}
    steps:
      - run: |
          d=$(printf '\044')
          want=$(printf '%s\n' "x' }} ${d}{{ github.job }} ${d}{{ '" '- run: evil' 'k: [a, {b: "c' '#')
          test "$GOT" = "$want"
          test "$Q" = "x' }} ${d}{{ secrets.OWN_SECRET }} ${d}{{ 'y"
          test "$B" = "x }} ${d}{{ secrets.OWN_SECRET }} y"
      - run: |
          d=$(printf '\044')
          cat > q <<'EOF'
          ${{ needs.out.outputs.q }}
          EOF
          cat > b <<'EOF'
          ${{ needs.out.outputs.b }}
          EOF
          test "$(cat q)" = "x' }} ${d}{{ secrets.OWN_SECRET }} ${d}{{ 'y"
          test "$(cat b)" = "x }} ${d}{{ secrets.OWN_SECRET }} y"
      - run: test "$(printf %s needs.out.outputs.v)" = needs.out.outputs.v
"#;

const STATE: &str = r#"
on: push
jobs:
  s:
    runs-on: ubuntu-latest
    steps:
      - run: |
          if [ -f plant ]; then
            echo planted > untracked.txt
            echo planted >> .gitignore
            mkdir -p .hidden build
            echo planted > .hidden/x
            echo planted > build/x
            echo planted > /home/runner/.shipcheck/planted
            echo planted > /tmp/planted
            git config user.planted yes
            git tag planted
            exit 0
          fi
          test ! -e untracked.txt
          ! grep -q planted .gitignore
          test ! -e .hidden
          test ! -e /tmp/planted
          test -z "$(git config user.planted)"
          ! git rev-parse -q --verify planted
          echo "build carried: $(test -f build/x && echo yes || echo no)"
          echo "volume carried: $(test -f /home/runner/.shipcheck/planted && echo yes || echo no)"
"#;

const HANG: &str = r#"
on: push
jobs:
  hang:
    runs-on: ubuntu-latest
    steps:
      - run: |
          nohup sh -c 'while :; do rm -rf .git; mkdir -p .git; printf "[core]\n\tfsmonitor = touch HOST_DIR/ran-timeout\n" > .git/config; sleep 0.2; done' > /dev/null 2>&1 &
          sleep 600
"#;

const ARTIFACTS: &str = "\
on: push
jobs:
  make:
    runs-on: ubuntu-latest
    steps:
      - run: mkdir out && echo built > out/file.txt
      - uses: actions/upload-artifact@v4
        with: {name: out, path: out}
  use:
    needs: make
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with: {name: out, path: got}
      - run: grep -q built got/file.txt
  probe:
    runs-on: ubuntu-latest
    steps:
      # bash succeeds once it connects, so a listener that never answers still counts as reached
      - run: timeout 10 bash -c 'echo > /dev/tcp/example.com/443'
      - run: \"! timeout 3 bash -c 'echo > /dev/tcp/127.0.0.1/HOST_PORT'\"
      - run: sleep 3
";

const SLOW_CI: &str = "\
on:
  push:
    branches: [main]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - run: test -f marker.txt
      - run: if [ -f slow ]; then sleep 30; fi
";

#[test]
fn status_names_the_result_for_the_working_copy_and_the_index() {
    let tmp = tempfile::tempdir().unwrap();
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
    // Host mode, so the plan's identity needs no act
    fs::write(
        work.join("shipcheck.toml"),
        "runner = \"host\"\n[host]\nchecks = { check = \"test -f marker.txt\" }\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);

    fs::write(work.join("marker.txt"), "staged").unwrap();
    git(&work, &["add", "marker.txt"]);
    fs::write(work.join("untracked.txt"), "left out").unwrap();

    // A passing result for the index's tree, as a `run --staged` leaves it
    let staged = run::snapshot(&work, true).unwrap().unwrap();
    let plan = run::plan(&work, &staged, "origin/main").unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join("cache")).unwrap();
    let entry = run::Entry {
        key: plan.key.clone(),
        sha: plan.sha.clone(),
        tree: plan.tree.clone(),
        base: plan.base.clone(),
        passed: true,
        workflows: Vec::new(),
        finished_unix: 0,
        skipped: None,
        ci_changes: Vec::new(),
    };
    fs::write(
        home.join("cache").join(format!("{}.json", plan.key)),
        serde_json::to_string(&entry).unwrap(),
    )
    .unwrap();

    let status = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_shipcheck"))
            .arg("status")
            .args(args)
            .env("SHIPCHECK_HOME", &home)
            .current_dir(&work)
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };

    let (code, out) = status(&[]);
    assert_eq!(code, Some(2), "the working copy has no result: {out}");
    assert!(out.contains("working copy, tree "), "{out}");
    assert!(
        out.contains(&format!("index, tree {}: passed", &plan.tree[..12])),
        "{out}"
    );
    assert_eq!(status(&["--staged"]).0, Some(0));

    // Committing the index as it is gives that tree, so its commit has the result
    git(&work, &["commit", "-q", "-m", "staged"]);
    assert_eq!(status(&["HEAD"]).0, Some(0));
}
