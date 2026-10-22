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

// `deps` checks the template was warmed from the developer's gitignored `deps/`, or on a volume
// that cannot clone, as CI's ext4, that the plain worktree has none. `env` passes only when the
// token stays out, `marker` only on a tree that has marker.txt. `jail` writes in its clone and its
// home, and tries the user's real home and a path outside the clone, which the test checks stay empty.
// On Linux /tmp is the sandbox's own, so a write there succeeds and lands nowhere
fn config(cow: bool, canaries: &[&Path]) -> String {
    let deps = if cow { "test -f" } else { "test ! -e" };
    let escapes: String = canaries
        .iter()
        .map(|c| format!("; touch '{}' 2>/dev/null", c.display()))
        .collect();
    format!(
        r#"runner = "host"
retries = 0
[host]
checks = {{ marker = "test -f marker.txt", deps = "{deps} deps/installed", env = "test -z \"$HOST_E2E_TOKEN\" && test \"$CI\" = true", jail = "touch \"$HOME/h\"{escapes}; touch inside.txt" }}
versions = ["echo tool 1.0"]
"#
    )
}

#[test]
fn runs_host_checks_caches_by_tree_and_commits() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary, and it sets the environment before any thread
    unsafe {
        std::env::set_var("SHIPCHECK_HOME", tmp.path().join("home"));
        std::env::set_var("HOST_E2E_TOKEN", "hunter2");
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
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let cow = git_cow::can_clone(&home, &home);
    let real_home = run::user_dir().join(format!(".shipcheck-canary-{}", std::process::id()));
    let outside = tmp.path().join("outside");
    fs::write(
        work.join("shipcheck.toml"),
        config(cow, &[&real_home, &outside]),
    )
    .unwrap();
    fs::write(work.join(".gitignore"), "deps/\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    // What `npm ci` leaves in the developer's checkout
    fs::create_dir(work.join("deps")).unwrap();
    fs::write(work.join("deps/installed"), "").unwrap();

    git(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("marker.txt"), "ok").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "feature"]);

    let plan = run::plan(&work, "feature", "origin/main").unwrap();
    assert!(run::cached(&plan.key).is_none());
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    let log = || fs::read_to_string(run::log_dir(&plan.key).join("host.log")).unwrap_or_default();
    assert!(entry.passed, "{entry:?}\n{}", log());
    assert_eq!(entry.workflows.len(), 1);
    assert_eq!(entry.workflows[0].file, "host");
    assert_eq!(entry.workflows[0].jobs.len(), 4);
    assert!(entry.workflows[0].jobs.values().all(|s| s == "success"));
    // The sandbox kept the check's writes in its clone
    assert!(!real_home.exists() && !outside.exists());
    assert!(!log().contains("hunter2"));
    // The checks ran in clones, never in the developer's checkout
    assert_eq!(git(&work, &["status", "--porcelain"]), "");

    // Another commit with the same tree reuses the result
    git(&work, &["commit", "-q", "--allow-empty", "-m", "same tree"]);
    let same = run::plan(&work, "HEAD", "origin/main").unwrap();
    assert_ne!(same.sha, plan.sha);
    assert_eq!(same.key, plan.key);
    assert!(run::lookup(&same).unwrap().passed);

    // A failing check fails the run, and names only that check
    let plan = run::plan(&work, "main", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    assert!(!entry.passed);
    assert_eq!(entry.workflows[0].jobs["marker"], "failure");
    assert_eq!(entry.workflows[0].jobs["deps"], "success");
    assert_eq!(entry.workflows[0].exit, Some(1));

    // The commit flow: run the working copy, commit exactly it, and the commit has the result
    let bin = Path::new(env!("CARGO_BIN_EXE_shipcheck"));
    let shipcheck = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .current_dir(&work)
            // CI has no git identity, and checks never see these
            .envs([
                ("GIT_AUTHOR_NAME", "t"),
                ("GIT_AUTHOR_EMAIL", "t@t"),
                ("GIT_COMMITTER_NAME", "t"),
                ("GIT_COMMITTER_EMAIL", "t@t"),
            ])
            .output()
            .unwrap()
    };
    fs::write(work.join("more.txt"), "x").unwrap();
    let out = shipcheck(&["run", "--no-fetch"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("host checks: passed"), "{text}");
    let out = shipcheck(&["commit", "-m", "more"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let committed = run::plan(&work, "HEAD", "origin/main").unwrap();
    assert!(run::lookup(&committed).unwrap().passed);

    // A change to what a check runs is named
    fs::write(work.join("Makefile"), "test:\n\ttrue\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "make"]);
    let plan = run::plan(&work, "HEAD", "origin/main").unwrap();
    assert!(
        plan.ci_changes
            .iter()
            .any(|c| c.contains("`Makefile` is added")),
        "{:?}",
        plan.ci_changes
    );
}
