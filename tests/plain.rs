use std::fs;
use std::process::{Command, Stdio};

// Agents parse this output, so without a terminal it carries no escape codes
#[test]
fn output_is_plain_without_a_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".github/workflows")).unwrap();
    fs::write(
        repo.join(".github/workflows/ci.yml"),
        "on: [push, pull_request]\njobs:\n  test:\n    steps:\n      - run: cargo test\n",
    )
    .unwrap();
    fs::write(
        repo.join("shipcheck.toml"),
        "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\n",
    )
    .unwrap();
    // init asks the remote for its default branch, so it needs one that answers
    let origin = tmp.path().join("origin.git");
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&[
        "init",
        "-q",
        "--bare",
        "-b",
        "main",
        origin.to_str().unwrap(),
    ]);
    git(&["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "a",
    ]);
    git(&["push", "-q", "origin", "main"]);

    for args in [&["init", "--yes"][..], &["doctor"], &["nope"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_shipcheck"))
            .args(args)
            .current_dir(&repo)
            .env("SHIPCHECK_HOME", tmp.path().join("home"))
            .env("SHIPCHECK_NO_DAEMON", "1")
            .env("SHIPCHECK_APP_ID", "1")
            .env_remove("NO_COLOR")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!said.contains('\x1b'), "{args:?}: {said:?}");
        assert!(!said.is_empty(), "{args:?}");

        if args[0] == "init" {
            assert!(said.contains("wrote .github/workflows/ci.yml"), "{said}");
        }
    }
}

// A fake git first on the PATH could report another tree, and so another tree's result
#[test]
fn a_fake_git_first_on_the_path_is_never_run() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, bin) = (tmp.path().join("repo"), tmp.path().join("bin"));
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let ran = tmp.path().join("fake-ran");
    fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\n/usr/bin/touch {}\nexec /usr/bin/git \"$@\"\n",
            ran.display()
        ),
    )
    .unwrap();
    fs::set_permissions(
        bin.join("git"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    Command::new(env!("CARGO_BIN_EXE_shipcheck"))
        .args(["status"])
        .current_dir(&repo)
        .env("SHIPCHECK_HOME", tmp.path().join("home"))
        .env("PATH", path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!ran.exists(), "shipcheck ran the fake git");
}

// Where shipcheck is installed can be a dir the user writes, such as ~/.cargo/bin, so it adds no dir to PATH
#[test]
fn a_fake_git_next_to_the_binary_is_never_run() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, bin) = (tmp.path().join("repo"), tmp.path().join("bin"));
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let ran = tmp.path().join("fake-ran");
    fs::copy(env!("CARGO_BIN_EXE_shipcheck"), bin.join("shipcheck")).unwrap();
    fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\n/usr/bin/touch {}\nexec /usr/bin/git \"$@\"\n",
            ran.display()
        ),
    )
    .unwrap();
    fs::set_permissions(
        bin.join("git"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    // /sbin is trusted and holds no git, so only a dir shipcheck added could find one
    let out = Command::new(bin.join("shipcheck"))
        .args(["status"])
        .current_dir(&repo)
        .env("SHIPCHECK_HOME", tmp.path().join("home"))
        .env("PATH", "/sbin")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!ran.exists(), "shipcheck ran the fake git: {out:?}");
}
