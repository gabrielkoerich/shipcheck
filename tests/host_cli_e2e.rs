//! The command line flows of the act end-to-end test that need no Docker, run in host mode through
//! the built binary, so `cargo test` catches a broken command before it reaches main

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Repo {
    _tmp: tempfile::TempDir,
    work: PathBuf,
    home: PathBuf,
}

impl Repo {
    /// A clone whose origin/main holds `config` as shipcheck.toml, on a new branch `feature`
    fn new(config: &str) -> Repo {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        git(
            tmp.path(),
            &["clone", "-q", &origin.to_string_lossy(), "work"],
        );
        let repo = Repo {
            work: tmp.path().join("work"),
            home: tmp.path().join("home"),
            _tmp: tmp,
        };
        repo.git(&["checkout", "-q", "-b", "main"]);
        // `shipcheck commit` runs plain `git commit`, and CI has no identity
        repo.git(&["config", "user.name", "t"]);
        repo.git(&["config", "user.email", "t@t"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        fs::write(repo.work.join("shipcheck.toml"), config).unwrap();
        repo.commit("main");
        repo.git(&["push", "-q", "origin", "main"]);
        repo.git(&["fetch", "-q", "origin"]);
        repo.git(&["switch", "-q", "-c", "feature"]);

        // `tmutil addexclusion` can take 10s a folder, so mark them excluded as it would
        for dir in ["templates", "runs", "hostcache"] {
            let dir = repo.home.join(dir);
            fs::create_dir_all(&dir).unwrap();

            if cfg!(target_os = "macos") {
                let _ = Command::new("xattr")
                    .args([
                        "-w",
                        "com.apple.metadata:com_apple_backup_excludeItem",
                        "com.apple.backupd",
                    ])
                    .arg(&dir)
                    .status();
            }
        }

        repo
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.work, args)
    }

    fn write(&self, file: &str, text: &str) {
        fs::write(self.work.join(file), text).unwrap();
    }

    fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// The exit code, and stdout then stderr
    fn shipcheck(&self, args: &[&str]) -> (Option<i32>, String) {
        let out = self.command(args).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_shipcheck"));
        cmd.args(args)
            .current_dir(&self.work)
            .env("SHIPCHECK_HOME", &self.home)
            .env("SHIPCHECK_NO_DAEMON", "1")
            .env_remove("SHIPCHECK_TOKEN");
        cmd
    }

    /// Stored results, which a run that passed writes and a `paths` skip never does
    fn cached(&self) -> usize {
        fs::read_dir(self.home.join("cache"))
            .into_iter()
            .flatten()
            .count()
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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

const MARKER: &str =
    "runner = \"host\"\nretries = 0\n[host]\nchecks = { marker = \"test -f marker.txt\" }\n";

// Nothing here starts a check, so it runs in shipcheck's own jail too
#[test]
fn status_logs_commit_and_post_refuse_before_any_run() {
    let repo = Repo::new(MARKER);
    repo.commit("off main");
    repo.write("marker.txt", "x");

    for args in [
        &["status"][..],
        &["status", "--no-fetch"],
        &["status", "feature"],
        &["status", "--staged"],
    ] {
        let (code, out) = repo.shipcheck(args);
        assert_eq!(code, Some(2), "{args:?}: {out}");
        assert!(out.contains("no result for tree"), "{args:?}: {out}");
    }

    let (code, out) = repo.shipcheck(&["logs", "--no-fetch"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("no logs for this tree"), "{out}");

    let (code, out) = repo.shipcheck(&["commit", "-m", "untested"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(
        out.contains("no `shipcheck run` of this working copy"),
        "{out}"
    );

    // The test's SHIPCHECK_HOME is not the default, so post refuses without --home-ok
    let (code, out) = repo.shipcheck(&["post"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("--home-ok"), "{out}");
    let (code, out) = repo.shipcheck(&["post", "--no-fetch", "--home-ok"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("no result for tree"), "{out}");

    // On the default branch both refuse before they commit or post anything
    fs::remove_file(repo.work.join("marker.txt")).unwrap();
    repo.git(&["switch", "-q", "main"]);
    let tip = repo.git(&["rev-parse", "HEAD"]);
    repo.write("on-main.txt", "x");
    let (code, out) = repo.shipcheck(&["commit", "-m", "on main"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(
        out.contains("main is the default branch") && out.contains("git switch -c"),
        "{out}"
    );
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), tip);
    let (code, out) = repo.shipcheck(&["post", "--home-ok"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("main is the default branch"), "{out}");
}

#[test]
#[cfg(target_os = "macos")]
#[cfg_attr(
    jail,
    ignore = "inside shipcheck's jail the outer sandbox is strict, so the checks run"
)]
fn inside_an_agents_sandbox_the_run_says_nothing_ran() {
    let repo = Repo::new(MARKER);
    repo.write("marker.txt", "x");
    // An agent's sandbox, which allows more than the host checks' own and so cannot hold it
    let out = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)(allow default)"])
        .arg(env!("CARGO_BIN_EXE_shipcheck"))
        .args(["run", "--no-fetch"])
        .current_dir(&repo.work)
        .env("SHIPCHECK_HOME", &repo.home)
        .env("SHIPCHECK_NO_DAEMON", "1")
        .env_remove("SHIPCHECK_TOKEN")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("cannot start inside another sandbox"),
        "{text}"
    );
    assert_eq!(repo.cached(), 0);
}

#[test]
fn run_then_commit_exactly_the_working_copy() {
    let repo = Repo::new(MARKER);
    repo.write("marker.txt", "tested before the commit");
    let before = repo.git(&["status", "--porcelain", "--ignored"]);
    let (code, out) = repo.shipcheck(&["run", "--no-fetch"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("tested the working copy, tree "), "{out}");
    assert!(
        out.contains("1 untracked file is part of this run: marker.txt"),
        "{out}"
    );
    assert_eq!(
        repo.git(&["status", "--porcelain", "--ignored"]),
        before,
        "the run must not write into the worktree"
    );
    let (code, out) = repo.shipcheck(&["status", "--no-fetch"]);
    assert_eq!(code, Some(0), "{out}");

    repo.write("late.txt", "after the run");
    let (code, out) = repo.shipcheck(&["commit", "-m", "late"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("files changed since the run"), "{out}");
    fs::remove_file(repo.work.join("late.txt")).unwrap();

    let (code, out) = repo.shipcheck(&["commit", "-m", "wip"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("the tree that passed"), "{out}");
    let sha = repo.git(&["rev-parse", "HEAD"]);
    assert_eq!(repo.git(&["log", "-1", "--format=%s"]), "wip");
    for rev in ["HEAD", "feature", sha.as_str()] {
        let (code, out) = repo.shipcheck(&["status", rev]);
        assert_eq!(
            code,
            Some(0),
            "the commit must find the run's result: {out}"
        );
    }

    // A named commit is tested as it is, the working copy does not count
    repo.write("marker.txt", "uncommitted on main");
    let (code, out) = repo.shipcheck(&["run", "--no-fetch", "main"]);
    assert_eq!(code, Some(1), "{out}");

    // A failed run of the working copy is not committed unless asked
    fs::remove_file(repo.work.join("marker.txt")).unwrap();
    let (code, out) = repo.shipcheck(&["run", "--no-fetch"]);
    assert_eq!(code, Some(1), "{out}");
    let (code, out) = repo.shipcheck(&["commit", "-m", "broken"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("--allow-failed"), "{out}");
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), sha);
    let (code, out) = repo.shipcheck(&["commit", "-m", "broken", "--allow-failed"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("the tree that failed"), "{out}");
    assert_eq!(repo.shipcheck(&["status", "HEAD"]).0, Some(1));
}

#[test]
fn run_staged_then_git_commit_reuses_the_result() {
    let repo = Repo::new(MARKER);
    repo.write("marker.txt", "staged");
    repo.git(&["add", "marker.txt"]);
    repo.write("not-staged.txt", "left out");
    let (code, out) = repo.shipcheck(&["run", "--no-fetch", "--staged"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("tested the index, tree "), "{out}");
    assert_eq!(repo.shipcheck(&["status", "--staged"]).0, Some(0));

    repo.git(&["commit", "-q", "-m", "staged"]);
    assert_eq!(
        repo.shipcheck(&["status", "HEAD"]).0,
        Some(0),
        "the index's commit must hit the cache"
    );

    // Another tree, with the file left out of the index, has no result
    repo.commit("more");
    assert_eq!(repo.shipcheck(&["status", "HEAD"]).0, Some(2));
}

// A branch that rewrites its own shipcheck.toml to pass still runs main's, and fails
#[test]
fn a_branch_cannot_pass_by_editing_its_own_config() {
    let repo = Repo::new(MARKER);
    repo.write(
        "shipcheck.toml",
        &MARKER.replace("test -f marker.txt", "true"),
    );
    repo.commit("make the check pass");
    let (code, out) = repo.shipcheck(&["run", "--no-fetch", "feature"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("host/marker: failure"), "{out}");
}

#[test]
fn two_runs_of_one_tree_share_one_run() {
    let repo = Repo::new(MARKER);
    repo.write("marker.txt", "x");
    repo.commit("marker");
    let spawn = || {
        repo.command(&["run", "--no-fetch"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (spawn(), spawn());
    let outs = [a.wait_with_output().unwrap(), b.wait_with_output().unwrap()];
    let texts: Vec<_> = outs
        .iter()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .collect();
    assert!(outs.iter().all(|o| o.status.success()), "{texts:?}");
    let ran = texts
        .iter()
        .filter(|t| t.contains("running, attempt"))
        .count();
    assert_eq!(
        ran, 1,
        "two requests for one tree must share one run: {texts:?}"
    );
}

#[test]
fn a_change_outside_paths_passes_and_is_never_cached() {
    let repo = Repo::new(&MARKER.replace("retries = 0\n", "retries = 0\npaths = [\"src/**\"]\n"));
    repo.write("README.md", "docs");
    let docs = repo.commit("docs only");
    let (code, out) = repo.shipcheck(&["status", &docs]);
    assert_eq!(code, Some(0), "a change outside paths must pass: {out}");
    assert!(
        out.contains("nothing to run: no change under paths"),
        "{out}"
    );
    let (code, out) = repo.shipcheck(&["run", "--no-fetch"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("nothing to run: no change under paths"),
        "{out}"
    );
    assert_eq!(repo.cached(), 0, "a paths skip must never be cached");

    // A change under paths runs, and its pass is cached
    fs::create_dir(repo.work.join("src")).unwrap();
    repo.write("src/lib.rs", "");
    repo.write("marker.txt", "x");
    repo.commit("code");
    let (code, out) = repo.shipcheck(&["run", "--no-fetch"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("running, attempt"), "{out}");
    assert_eq!(repo.cached(), 1);
}

// `shipcheck logs` hands no control character from a check to the terminal
#[test]
fn logs_print_a_run_without_control_characters() {
    let repo = Repo::new(
        "runner = \"host\"\nretries = 0\n[host]\nchecks = { title = \"printf '\\\\033]0;from-check\\\\007\\\\033[2J\\\\n'\" }\n",
    );
    repo.commit("empty");
    let (code, out) = repo.shipcheck(&["run", "--no-fetch", "feature"]);
    assert_eq!(code, Some(0), "{out}");
    let logs = repo
        .command(&["logs", "--no-fetch", "feature"])
        .output()
        .unwrap();
    let printed = String::from_utf8_lossy(&logs.stdout);
    assert!(logs.status.success(), "{printed}");
    assert!(printed.contains("\\u{1b}]0;from-check"), "{printed}");
    assert!(!logs.stdout.contains(&0x1b) && !logs.stdout.contains(&0x07));
}

// With commit in `on`, each commit runs in the background and cancels the run before it
#[test]
fn on_commit_runs_in_the_background_and_a_newer_commit_cancels() {
    let repo = Repo::new(
        "runner = \"host\"\nretries = 0\non = [\"push\", \"commit\"]\n[host]\nchecks = { marker = \"test -f marker.txt && if [ -f slow ]; then sleep 30; fi\" }\n",
    );
    let committed = |message: &str| {
        let sha = repo.commit(message);
        let (code, out) = repo.shipcheck(&["on-commit"]);
        assert_eq!(
            code,
            Some(0),
            "the commit hook must never fail a commit: {out}"
        );
        sha
    };
    let log = || fs::read_to_string(repo.home.join("logs/background.log")).unwrap_or_default();
    repo.write("marker.txt", "slow");
    repo.write("slow", "");
    let slow = committed("slow");
    std::thread::sleep(Duration::from_secs(3));
    fs::remove_file(repo.work.join("slow")).unwrap();
    let fast = committed("fast");
    let deadline = Instant::now() + Duration::from_secs(120);

    while repo.shipcheck(&["status", "--no-fetch"]).0 != Some(0) {
        assert!(
            Instant::now() < deadline,
            "the newer commit must pass: {}",
            log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    assert_eq!(repo.shipcheck(&["status", &fast]).0, Some(0));
    assert_eq!(
        repo.shipcheck(&["status", &slow]).0,
        Some(2),
        "the cancelled run must leave no result: {}",
        log()
    );
}

#[test]
fn clean_removes_this_repository_and_all_the_results() {
    let repo = Repo::new(MARKER);
    repo.write("marker.txt", "x");
    repo.commit("marker");
    assert_eq!(repo.shipcheck(&["run", "--no-fetch"]).0, Some(0));
    let templates = repo.home.join("templates");
    assert!(templates.read_dir().unwrap().next().is_some());

    let (code, out) = repo.shipcheck(&["clean"]);
    assert_eq!(code, Some(0), "{out}");
    // Templates sit at templates/<owner>/<repo>, and this repository is `origin`
    let left: Vec<_> = fs::read_dir(&templates)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|owner| fs::read_dir(owner.path()).into_iter().flatten().flatten())
        .map(|e| e.file_name())
        .filter(|name| name.to_string_lossy().contains("origin"))
        .collect();
    assert!(left.is_empty(), "clean left the template: {left:?}");
    assert!(!repo.git(&["worktree", "list"]).contains("templates"));
    assert_eq!(repo.cached(), 1, "clean keeps the results");

    let (code, out) = repo.shipcheck(&["clean", "--all"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(!repo.home.join("cache").exists(), "{out}");
    assert!(out.contains("daemon service left alone"), "{out}");
}
