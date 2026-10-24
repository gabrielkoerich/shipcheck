//! Each run builds and tests its own tree, never build output another tree left in the shared
//! template, through the real binary with a home of its own

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

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

fn write(dir: &Path, name: &str, text: &str) {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn shipcheck(home: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shipcheck"))
        .args(args)
        .env("SHIPCHECK_HOME", home)
        .env("SHIPCHECK_NO_DAEMON", "1")
        .current_dir(dir)
        .output()
        .unwrap()
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_run_never_uses_build_output_another_tree_left() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let origin = tmp.path().join("acme/demo.git");
    fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(
        tmp.path(),
        &["clone", "-q", &origin.to_string_lossy(), "main"],
    );
    let main = tmp.path().join("main");
    git(&main, &["checkout", "-q", "-b", "main"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    write(
        &main,
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&main, "src/lib.rs", "pub mod a;\n");
    write(&main, "src/a.rs", "#[test]\nfn on_main() {}\n");
    write(&main, "src/main.rs", "fn main() {}\n");
    // Starts the crate's binary at the path cargo built into the test
    write(
        &main,
        "tests/bin.rs",
        "#[test]\nfn binary_runs() {\n    std::process::Command::new(env!(\"CARGO_BIN_EXE_demo\")).status().unwrap();\n}\n",
    );
    write(
        &main,
        "shipcheck.toml",
        "runner = \"host\"\nretries = 0\n[host]\nchecks = { test = \"cargo test\" }\n",
    );
    write(&main, ".gitignore", "target\n");
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "main"]);
    git(&main, &["push", "-q", "origin", "main"]);
    git(&main, &["fetch", "-q", "origin"]);

    // Branch a adds a test, branch b leaves src/ as main has it
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "a",
            &a.to_string_lossy(),
            "main",
        ],
    );
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "b",
            &b.to_string_lossy(),
            "main",
        ],
    );
    write(
        &a,
        "src/a.rs",
        "#[test]\nfn on_main() {}\n#[test]\nfn only_in_branch_a() {}\n",
    );
    git(&a, &["commit", "-qam", "a"]);
    write(&b, "README.md", "b\n");
    git(&b, &["add", "-A"]);
    git(&b, &["commit", "-qm", "b"]);

    let ran_a = shipcheck(&home, &a, &["run", "--no-fetch"]);
    assert!(ran_a.status.success(), "{}", said(&ran_a));
    let log_a = said(&shipcheck(&home, &a, &["logs"]));
    assert!(log_a.contains("only_in_branch_a"), "{log_a}");

    // b runs in another slot while slot 0 is held, so a test binary built there would point at it
    let slots = home.join("runs/acme/demo");
    fs::create_dir_all(&slots).unwrap();
    let held = fs::File::create(slots.join("0.lock")).unwrap();
    held.lock().unwrap();
    let ran_b = shipcheck(&home, &b, &["run", "--no-fetch"]);
    drop(held);
    let out = said(&ran_b) + &said(&shipcheck(&home, &b, &["logs"]));
    assert!(
        !out.contains("only_in_branch_a"),
        "b ran a test only branch a has, from a's build:\n{out}"
    );
    assert!(
        ran_b.status.success(),
        "b's own tests must pass in its own slot:\n{out}"
    );
}
