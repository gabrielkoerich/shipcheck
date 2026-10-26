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

/**
 * The check leaves a loop in a process group of its own, which waits until the slot's next
 * checkout is verified and has its own `.git`, then writes the file the check tests for. Run on a
 * tree without that file, the check must still fail
 **/
const CHECK: &str = r#"sleep 1; test -f marker.txt; r=$?; d=$PWD; set -m; ( i=0; while [ $i -lt 400 ]; do if [ -d "$d/.git" ] && [ ! -e "$d/marker.txt" ]; then echo ok > "$d/marker.txt"; fi; i=$((i+1)); sleep 0.05; done ) & exit $r"#;

#[test]
fn a_process_a_check_leaves_cannot_change_the_next_run() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary, and it sets the environment before any thread
    unsafe {
        std::env::set_var("SHIPCHECK_HOME", tmp.path().join(".local/state/shipcheck"));
    }

    if shipcheck::runner::host::in_a_jail() {
        // A jail of shipcheck's own tests has no launchd to reach, and its checks never post
        return;
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
    let config =
        format!("runner = \"host\"\nretries = 0\n[host]\nchecks = {{ marker = '''{CHECK}''' }}\n");
    fs::write(work.join("shipcheck.toml"), config).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "main"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);

    git(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("marker.txt"), "ok").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "passes"]);
    let plan = run::plan(&work, "feature", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    assert!(entry.passed, "{entry:?}");

    git(&work, &["rm", "-q", "marker.txt"]);
    git(&work, &["commit", "-q", "-m", "fails"]);
    let plan = run::plan(&work, "feature", "origin/main").unwrap();
    let entry = run::run(&plan, &mut |l| eprintln!("{l}")).unwrap();
    let log = fs::read_to_string(run::log_dir(&plan.key).join("host.log")).unwrap_or_default();
    assert!(
        !entry.passed,
        "the first run's leftover passed this tree\n{log}"
    );
    assert_eq!(entry.workflows[0].jobs["marker"], "failure", "{log}");
}
