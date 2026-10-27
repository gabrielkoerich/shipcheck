use std::fs;
use std::process::Command;

// The base asks for sandbox = false, and only `shipcheck trust` on this machine lets it out
#[test]
fn a_check_with_sandbox_false_stays_in_the_sandbox_until_the_repository_is_trusted() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("repo"), tmp.path().join("home"));
    let origin = tmp.path().join("origin.git");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("shipcheck.toml"),
        "runner = \"host\"\n[host.checks]\nlint = \"true\"\n[host.checks.ios]\nrun = \"true\"\nsandbox = false\n",
    )
    .unwrap();
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
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "-m",
        "a",
    ]);
    git(&["push", "-q", "origin", "main"]);
    let shipcheck = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_shipcheck"))
            .args(args)
            .current_dir(&repo)
            .env("SHIPCHECK_HOME", &home)
            .env("SHIPCHECK_NO_DAEMON", "1")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    };

    let before = shipcheck(&["doctor"]);
    assert!(
        before.contains("ios: this repository asks to run it with sandbox = false"),
        "{before}"
    );
    assert!(!before.contains("runs outside the sandbox"), "{before}");

    assert!(shipcheck(&["trust"]).contains("trusted"));
    let after = shipcheck(&["doctor"]);
    assert!(after.contains("ios runs outside the sandbox"), "{after}");
    assert!(!after.contains("asks to run it"), "{after}");

    shipcheck(&["trust", "--remove"]);
    assert!(shipcheck(&["doctor"]).contains("asks to run it"));
}
