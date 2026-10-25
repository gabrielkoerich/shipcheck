//! `init --yes` tries each host check it would write in the jail, through the built binary

use std::fs;
use std::path::Path;
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

#[test]
fn init_writes_only_the_checks_that_passed_its_trial() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let work = tmp.path().join("work");
    fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(
        tmp.path(),
        &["clone", "-q", &origin.to_string_lossy(), "work"],
    );
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

    let home = tmp.path().join("home");

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

    let out = Command::new(env!("CARGO_BIN_EXE_shipcheck"))
        .args(["init", "--yes"])
        .current_dir(&work)
        .env("SHIPCHECK_HOME", &home)
        .env("SHIPCHECK_NO_DAEMON", "1")
        .env("SHIPCHECK_APP_ID", "1")
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
