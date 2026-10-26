//! `init --yes` tries each host check it would write in the jail, through the built binary

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CI: &str = "on: [push, pull_request]
jobs:
  plain:
    runs-on: ubuntu-latest
    steps:
      - run: ./check.sh plain
  colored:
    runs-on: ubuntu-latest
    steps:
      - run: ./check.sh --color=always
  missing:
    runs-on: ubuntu-latest
    steps:
      - run: shipcheck-missing-tool --check
";

// Fails as a tool does that refuses to force color without a terminal
const CHECK: &str = "#!/bin/sh
for a in \"$@\"; do
  if [ \"$a\" = --color=always ]; then
    echo 'no terminal to color' >&2
    exit 1
  fi
done
";

fn git(dir: &Path, args: &[&str]) {
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
}

/// A clone of a bare origin on `main` with CI's jobs pushed, and a home for shipcheck
fn repo(tmp: &Path) -> (PathBuf, PathBuf) {
    let origin = tmp.join("origin.git");
    let work = tmp.join("work");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(tmp, &["clone", "-q", &origin.to_string_lossy(), "work"]);
    git(&work, &["checkout", "-q", "-b", "main"]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["config", "user.email", "t@t"]);
    git(&work, &["config", "commit.gpgsign", "false"]);
    fs::create_dir_all(work.join(".github/workflows")).unwrap();
    fs::write(work.join(".github/workflows/ci.yml"), CI).unwrap();
    fs::write(work.join("check.sh"), CHECK).unwrap();
    let mode = <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755);
    fs::set_permissions(work.join("check.sh"), mode).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "ci"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["remote", "set-head", "origin", "main"]);

    let home = tmp.join("home");

    // `tmutil addexclusion` can take 10s a folder, so mark them excluded as it would
    for dir in ["templates", "runs", "hostcache"] {
        let dir = home.join(dir);
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

    (work, home)
}

/// `shipcheck init` with these arguments and no terminal, its output, failing the test when it fails
fn init(work: &Path, home: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_shipcheck"))
        .arg("init")
        .args(args)
        .current_dir(work)
        .env("SHIPCHECK_HOME", home)
        .env("SHIPCHECK_NO_DAEMON", "1")
        .env("SHIPCHECK_APP_ID", "1")
        // No Docker on this PATH, so a failed check stays on GitHub rather than going to act
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env_remove("SHIPCHECK_TOKEN")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}");
    said
}

/// Every file under `dir` with its contents, and every directory
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();

    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();

        if path.is_dir() {
            out.insert(path.clone(), None);
            out.extend(snapshot(&path));
        } else {
            out.insert(path.clone(), fs::read(&path).ok());
        }
    }

    out
}

#[test]
fn init_without_a_terminal_or_yes_shows_the_plan_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, home) = repo(tmp.path());
    let (repo_before, home_before) = (snapshot(&work), snapshot(&home));
    let said = init(&work, &home, &[]);
    assert!(said.contains("plain ... passed"), "{said}");
    assert!(said.contains("Review"), "{said}");
    assert!(said.contains("rerun with --yes to apply"), "{said}");
    let changed = |before: &BTreeMap<PathBuf, Option<Vec<u8>>>,
                   after: BTreeMap<PathBuf, Option<Vec<u8>>>| {
        let keys: std::collections::BTreeSet<_> =
            before.keys().chain(after.keys()).cloned().collect();
        keys.into_iter()
            .filter(|k| before.get(k) != after.get(k))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        changed(&home_before, snapshot(&home)),
        Vec::<PathBuf>::new(),
        "SHIPCHECK_HOME changed"
    );
    assert_eq!(
        changed(&repo_before, snapshot(&work)),
        Vec::<PathBuf>::new(),
        "the repository changed"
    );
}

#[test]
fn init_writes_only_the_checks_that_passed_its_trial() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, home) = repo(tmp.path());
    let said = init(&work, &home, &["--yes"]);

    let config = fs::read_to_string(work.join("shipcheck.toml")).unwrap();
    assert!(config.contains("plain = \"./check.sh plain\""), "{config}");
    assert!(config.contains("colored = \"./check.sh\""), "{config}");
    assert!(!config.contains("missing"), "{config}");
    assert!(said.contains("plain ... passed"), "{said}");
    assert!(said.contains("colored ... passed"), "{said}");
    assert!(said.contains("missing ... failed"), "{said}");
    assert!(
        said.contains("job missing: failed when init tried it:")
            && said.contains("fix: install shipcheck-missing-tool"),
        "{said}"
    );

    // The jobs shipcheck runs are gated to main, the one that failed keeps running on PRs
    let ci = fs::read_to_string(work.join(".github/workflows/ci.yml")).unwrap();
    let job = |id: &str| -> String {
        ci.lines()
            .skip_while(|l| *l != format!("  {id}:"))
            .skip(1)
            .take_while(|l| l.starts_with("    "))
            .collect()
    };
    assert!(job("plain").contains("if:"), "{ci}");
    assert!(job("colored").contains("if:"), "{ci}");
    assert!(!job("missing").contains("if:"), "{ci}");
    assert!(
        !home.join("cache").exists() || fs::read_dir(home.join("cache")).unwrap().next().is_none(),
        "a trial caches nothing"
    );
    let logs: Vec<_> = fs::read_dir(home.join("logs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(logs.is_empty(), "a trial keeps no logs: {logs:?}");
}

#[test]
fn a_trial_tries_the_host_checks_and_never_an_act_job() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["config", "user.email", "t@t"]);
    git(&work, &["config", "commit.gpgsign", "false"]);
    git(&work, &["remote", "add", "origin", &work.to_string_lossy()]);
    fs::write(work.join("a.txt"), "a").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "a"]);
    // An act job that names no job would fail the whole run if the trial ran it
    let config = shipcheck::config::RepoConfig::parse(
        "runner = \"host\"\n[host.checks]\nok = \"true\"\n[jobs.\"ci.yml/missing\"]\nrunner = \"act\"\n",
    )
    .unwrap();
    // SAFETY: the only test in this binary that sets the environment, the other passes its own to a child
    unsafe { std::env::set_var("SHIPCHECK_HOME", tmp.path().join("home")) };
    let tried = shipcheck::run::trial(&work, config, &mut |_| {}).unwrap();
    assert_eq!(tried.keys().collect::<Vec<_>>(), ["ok"]);
    assert!(tried["ok"].passed, "{}", tried["ok"].log);
}
