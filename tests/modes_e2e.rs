mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

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

/// A clone on `feature` of an origin whose main holds `config` and `files`
fn repo(tmp: &Path, config: &str, files: &[(&str, &str)]) -> PathBuf {
    let origin = tmp.join("origin.git");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(tmp, &["clone", "-q", &origin.to_string_lossy(), "work"]);
    let work = tmp.join("work");
    git(&work, &["checkout", "-q", "-b", "main"]);
    fs::write(work.join("shipcheck.toml"), config).unwrap();

    for (path, text) in files {
        let path = work.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);
    work
}

fn commit(work: &Path, files: &[&str]) {
    for f in files {
        fs::write(work.join(f), "ok").unwrap();
    }

    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "--allow-empty", "-m", "change"]);
}

fn shipcheck(work: &Path, home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_shipcheck"));
    cmd.arg("run")
        .args(args)
        .current_dir(work)
        .env("SHIPCHECK_HOME", home)
        .env("SHIPCHECK_NO_DAEMON", "1");
    cmd
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Passed results in the home's cache, which only a full run that passed writes
fn passes(home: &Path) -> usize {
    fs::read_dir(home.join("cache"))
        .map(|d| {
            d.filter_map(|e| fs::read(e.ok()?.path()).ok())
                .filter_map(|t| serde_json::from_slice::<serde_json::Value>(&t).ok())
                .filter(|e| e["passed"] == true)
                .count()
        })
        .unwrap_or(0)
}

/// Sends SIGINT, as Ctrl-C does, once `ready` holds, and waits for the run to end
fn interrupt(mut run: Child, ready: impl Fn() -> bool) -> std::process::ExitStatus {
    let started = Instant::now();

    while !ready() {
        assert!(
            run.try_wait().unwrap().is_none(),
            "the run ended before it was interrupted"
        );
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "the run never got going"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    let pid = run.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap()
            .success()
    );
    run.wait().unwrap()
}

/// The processes whose command line holds `marker`
fn running(marker: &str) -> Vec<String> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(marker) && !l.contains("ps -axo"))
        .map(String::from)
        .collect()
}

/// The containers of the workflows below, whose name `modes` act puts in every container name
fn containers() -> Vec<String> {
    let out = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "name=^act-modes-",
            "--format",
            "{{.Names}}",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect()
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();

    while !done() {
        assert!(started.elapsed() < Duration::from_secs(60), "{what}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

// A unique sleep, so the test finds the check's own process by its command line
const HANG: &str = "sleep 7391";

const HOST: &str = "\
runner = \"host\"
retries = 0

[host.checks]
test = \"test -f marker.txt\"
hang = \"if [ -f hang.txt ]; then sleep 7391; fi\"
";

#[test]
#[ignore = "reads ps, which shipcheck's own host sandbox denies"]
fn a_host_run_interrupted_leaves_no_process_and_no_result() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = repo(tmp.path(), HOST, &[]);
    commit(&work, &["marker.txt", "hang.txt"]);

    let run = shipcheck(&work, &home, &[]).spawn().unwrap();
    let status = interrupt(run, || !running(HANG).is_empty());
    assert!(!status.success());
    wait_until("the check outlived shipcheck", || running(HANG).is_empty());
    assert_eq!(passes(&home), 0, "an interrupted run stores no pass");

    // A partial run that passes stores nothing either
    fs::remove_file(work.join("hang.txt")).unwrap();
    commit(&work, &[]);
    let out = shipcheck(&work, &home, &["--job", "test"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    assert!(said(&out).contains("partial run: not the check"));
    assert_eq!(passes(&home), 0, "a partial run stores no result");
}

#[test]
#[ignore = "takes a minute, the shortest timeout"]
fn a_host_check_past_its_timeout_fails_and_is_killed() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config = HOST.replace("retries = 0\n", "retries = 0\ntimeout_minutes = 1\n");
    let work = repo(tmp.path(), &config, &[]);
    commit(&work, &["marker.txt", "hang.txt"]);

    let out = shipcheck(&work, &home, &["--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", said(&out));
    let entry: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let state = |id: &str| {
        entry["workflows"][0]["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == id)
            .unwrap()["status"]
            .clone()
    };
    assert_eq!(state("hang"), "timed out");
    assert_eq!(state("test"), "success");
    assert!(running(HANG).is_empty(), "{:?}", running(HANG));
    assert_eq!(passes(&home), 0);
}

// A matrix whose second leg reads leg2.txt, a job that needs the matrix, and `test`, whose host
// check runs in its place. `hang.txt` makes a job sleep
const CI: &str = "\
name: modes
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - run: exit 1
  legs:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        n: [1, 2]
    steps:
      - run: test \"${{ matrix.n }}\" = 1 || test -f leg2.txt
  after:
    needs: legs
    runs-on: ubuntu-latest
    steps:
      - run: test -f marker.txt
  hang:
    runs-on: ubuntu-latest
    steps:
      - run: if [ -f hang.txt ]; then sleep 600; fi
";

const LISTED: &str = "\
runner = \"act\"
workflows = [\"ci.yml\"]
retries = 0

[host.checks]
test = \"test -f marker.txt\"

[jobs.\"ci.yml/test\"]
runner = \"host\"

[act]
cache_paths = []
";

#[test]
#[ignore = "needs act and Docker"]
fn a_host_check_takes_a_jobs_place_in_a_listed_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _clean = common::CleanOnDrop::new(&home);
    let work = repo(tmp.path(), LISTED, &[(".github/workflows/ci.yml", CI)]);
    let run = |args: &[&str]| -> (Option<i32>, serde_json::Value) {
        let out = shipcheck(&work, &home, &[&["--json"], args].concat())
            .output()
            .unwrap();
        let entry =
            serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", said(&out)));
        (out.status.code(), entry)
    };
    let states = |entry: &serde_json::Value| -> Vec<String> {
        entry["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|w| {
                w["jobs"].as_array().unwrap().iter().map(move |j| {
                    format!(
                        "{}/{}={}",
                        w["file"].as_str().unwrap(),
                        j["id"].as_str().unwrap(),
                        j["status"].as_str().unwrap()
                    )
                })
            })
            .collect()
    };

    commit(&work, &["marker.txt", "leg2.txt"]);
    let (code, entry) = run(&[]);
    assert_eq!(code, Some(0), "{entry}");
    assert_eq!(
        states(&entry),
        [
            "host/test=success",
            "ci.yml/after=success",
            "ci.yml/hang=success",
            "ci.yml/legs__1=success",
            "ci.yml/legs__2=success",
        ]
    );
    assert_eq!(passes(&home), 1);

    // One failed leg fails the run, and the job that needs the matrix never runs
    fs::remove_file(work.join("leg2.txt")).unwrap();
    commit(&work, &[]);
    let (code, entry) = run(&[]);
    assert_eq!(code, Some(1), "{entry}");
    let got = states(&entry);
    assert!(got.contains(&"ci.yml/legs__1=success".into()), "{got:?}");
    assert!(got.contains(&"ci.yml/legs__2=failure".into()), "{got:?}");
    assert!(
        got.contains(&"ci.yml/after=not run, a job it needs did not pass".into()),
        "{got:?}"
    );
    assert!(got.contains(&"host/test=success".into()), "{got:?}");

    // --job picks the matrix's every leg, and a partial pass stores nothing
    let (code, entry) = run(&["--job", "after"]);
    assert_eq!(code, Some(1), "{entry}");
    assert_eq!(states(&entry).len(), 3, "after and both legs");
    commit(&work, &["leg2.txt"]);
    let (code, entry) = run(&["--job", "legs"]);
    assert_eq!(code, Some(0), "{entry}");
    assert_eq!(
        states(&entry),
        ["ci.yml/legs__1=success", "ci.yml/legs__2=success"]
    );
    assert_eq!(passes(&home), 1, "a partial run stores no result");

    // Interrupted while a job runs, the run leaves no container and no result
    commit(&work, &["hang.txt"]);
    let child = shipcheck(&work, &home, &[]).spawn().unwrap();
    let status = interrupt(child, || containers().iter().any(|c| c.contains("hang")));
    assert!(!status.success());
    wait_until("a job container outlived shipcheck", || {
        containers().is_empty()
    });
    assert_eq!(passes(&home), 1, "an interrupted run stores no pass");
}

#[test]
#[ignore = "needs act and Docker, and takes a minute"]
fn an_act_job_past_its_timeout_fails_and_leaves_no_container() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _clean = common::CleanOnDrop::new(&home);
    let config = LISTED.replace("retries = 0\n", "retries = 0\ntimeout_minutes = 1\n");
    let work = repo(tmp.path(), &config, &[(".github/workflows/ci.yml", CI)]);
    commit(&work, &["marker.txt", "leg2.txt", "hang.txt"]);

    let out = shipcheck(&work, &home, &[]).output().unwrap();
    let text = said(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("ci.yml/hang: timed out"), "{text}");
    assert!(containers().is_empty(), "{:?}", containers());
    assert_eq!(passes(&home), 0);
}
