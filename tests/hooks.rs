//! The commands the git hooks call, through the real binary with a home of their own

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

/// Every record line under the home's `background/`, from any repository and branch
fn records(home: &Path) -> String {
    let mut text = String::new();
    let mut dirs = vec![home.join("background")];

    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                dirs.push(entry.path());
            } else {
                text += &fs::read_to_string(entry.path()).unwrap_or_default();
            }
        }
    }

    text
}

/// A clone whose main has a host check, on a branch `feature` one commit ahead, and its commit
fn clone_with_feature(tmp: &Path) -> (PathBuf, String) {
    let origin = tmp.join("origin.git");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(tmp, &["clone", "-q", &origin.to_string_lossy(), "work"]);
    let work = tmp.join("work");
    git(&work, &["checkout", "-q", "-b", "main"]);
    git(&work, &["config", "user.email", "t@example.com"]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["config", "commit.gpgsign", "false"]);
    fs::write(
        work.join("shipcheck.toml"),
        "runner = \"host\"\nretries = 0\n[host]\nchecks = { ok = \"true\" }\n",
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "config"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);
    git(&work, &["switch", "-q", "-c", "feature"]);
    fs::write(work.join("a.txt"), "a").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "a"]);
    let sha = git(&work, &["rev-parse", "HEAD"]);
    (work, sha)
}

#[test]
fn on_push_runs_the_pushed_commit_and_on_commit_only_when_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let (work, sha) = clone_with_feature(tmp.path());

    let shipcheck = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_shipcheck"))
            .args(args)
            .env("SHIPCHECK_HOME", &home)
            .env("SHIPCHECK_NO_DAEMON", "1")
            .current_dir(&work)
            .output()
            .unwrap()
    };

    // The pre-push hook's call returns at once and leaves a run of the pushed commit
    let pushed = shipcheck(&["on-push", "refs/heads/feature", &sha]);
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(120);

    while !shipcheck(&["status", &sha]).status.success() {
        assert!(
            Instant::now() < deadline,
            "on-push must start a run that caches a result. Records:\n{}\nLog:\n{}",
            records(&home),
            fs::read_to_string(home.join("logs/background.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // With the default `on = ["push"]` a commit starts nothing
    fs::write(work.join("b.txt"), "b").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "b"]);
    let committed = git(&work, &["rev-parse", "HEAD"]);
    assert!(shipcheck(&["on-commit"]).status.success());
    assert!(
        !records(&home).contains(&committed),
        "on-commit ran though on does not list commit: {}",
        records(&home)
    );
}

/// A fake shipcheck service that refuses every call as the real one does for a repository the App
/// is not on while `refuse` holds, and accepts every call after
fn fake_service(refuse: Arc<AtomicBool>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(&stream);
            let mut length = 0;

            loop {
                let mut line = String::new();

                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }

                if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
                    length = n.trim().parse().unwrap_or(0);
                }
            }

            let _ = reader.read_exact(&mut vec![0; length]);
            let (status, body) = if refuse.load(Ordering::SeqCst) {
                (
                    "403 Forbidden",
                    r#"{"error":"the shipcheck App is not installed on acme/app, or you have no access to it, install it from https://github.com/apps/shipcheck-run"}"#,
                )
            } else {
                (
                    "200 OK",
                    r#"{"id":1,"html_url":"https://github.com/acme/app/runs/1"}"#,
                )
            };
            let _ = write!(
                &stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });

    url
}

#[test]
fn a_refused_post_shows_in_status_history_logs_and_the_hook_until_a_post_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let (work, sha) = clone_with_feature(tmp.path());
    let refuse = Arc::new(AtomicBool::new(true));
    let api = fake_service(refuse.clone());
    let command = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_shipcheck"));
        c.args(args)
            .env("SHIPCHECK_HOME", &home)
            .env("SHIPCHECK_NO_DAEMON", "1")
            .env("SHIPCHECK_API", &api)
            .env("SHIPCHECK_TOKEN", "shc_test")
            .env("SHIPCHECK_TRIGGER", "hook-push")
            .current_dir(&work);
        c
    };
    let text = |out: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };

    // A moved home never posts from the hook, but its run leaves the record directory to find
    assert!(
        command(&["on-push", "refs/heads/feature", &sha])
            .output()
            .unwrap()
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(120);

    while !command(&["status", &sha])
        .output()
        .unwrap()
        .status
        .success()
    {
        assert!(Instant::now() < deadline, "no result: {}", records(&home));
        std::thread::sleep(Duration::from_millis(200));
    }

    let dir = fs::read_dir(home.join("background"))
        .unwrap()
        .flatten()
        .map(|owner| {
            fs::read_dir(owner.path())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path()
        })
        .next()
        .unwrap();

    // A run that posts, listed in the record as the hook lists the one it starts
    let post = || {
        let child = command(&[
            "run",
            "--no-fetch",
            "--sha",
            &sha,
            "--base",
            "origin/main",
            "--background",
            "feature",
            "--home-ok",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
        fs::write(
            dir.join("feature.pid"),
            format!("{} {sha} post\n", child.id()),
        )
        .unwrap();
        child.wait_with_output().unwrap()
    };
    let refused = post();
    assert!(!refused.status.success(), "{}", text(&refused));
    let repo = format!(
        "{}/{}",
        dir.parent().unwrap().file_name().unwrap().to_string_lossy(),
        dir.file_name().unwrap().to_string_lossy()
    );
    let line = format!(
        "checks not posted: the shipcheck App is not installed on {repo}, fix: add {repo} to the shipcheck App: https://github.com/settings/installations (shipcheck-run, Configure)"
    );
    assert!(text(&refused).contains(&line), "{}", text(&refused));

    let status = command(&["status", &sha]).output().unwrap();
    assert!(text(&status).contains(&line), "{}", text(&status));

    let hook = command(&["on-push", "refs/heads/feature", &sha])
        .output()
        .unwrap();
    assert!(
        hook.status.success(),
        "the hook never fails git: {}",
        text(&hook)
    );
    assert!(text(&hook).contains(&line), "{}", text(&hook));

    let history = text(&command(&["history"]).output().unwrap());
    assert!(
        history
            .lines()
            .last()
            .unwrap()
            .ends_with("not posted (no App)"),
        "{history}"
    );
    let json = text(&command(&["history", "--json"]).output().unwrap());
    assert!(
        json.contains("the shipcheck App is not installed on acme/app"),
        "{json}"
    );

    let logs = text(&command(&["logs", &sha]).output().unwrap());
    let posting = format!(
        "posting: not posted, the shipcheck App is not installed on {repo}, fix: add {repo} to the shipcheck App"
    );
    assert!(logs.contains(&posting), "{logs}");

    // Once the App is added, the next post clears it
    refuse.store(false, Ordering::SeqCst);
    let posted = post();
    assert!(posted.status.success(), "{}", text(&posted));

    for args in [
        &["status", &sha][..],
        &["on-push", "refs/heads/feature", &sha],
    ] {
        let out = text(&command(args).output().unwrap());
        assert!(!out.contains("checks not posted"), "{out}");
    }

    let history = text(&command(&["history"]).output().unwrap());
    assert!(
        history.lines().last().unwrap().ends_with(" posted"),
        "{history}"
    );
    let logs = text(&command(&["logs", &sha]).output().unwrap());
    assert!(logs.contains("posting: posted at "), "{logs}");
    assert!(logs.trim_end().ends_with(" by hook-push"), "{logs}");
}
