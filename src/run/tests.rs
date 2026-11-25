use super::checkout::{job_git, remove_planted_links, standalone_git, verify_checkout};
use super::plan::{act_jobs, ci_changes, copy_index, env_skips};
use super::slots::{QueueStep, Ticket, claim_slot, queue_step};
use super::*;
use crate::base::remote_base;
use crate::git::safe_git_env;
use crate::paths::{default_home, home, home_overridden, log_dir, user_dir};
use crate::ui::printable;
use crate::{history, post::check};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;

#[test]
fn keeps_passed_jobs_that_no_job_running_again_needs() {
    let graph: Vec<(String, Vec<String>)> = [
        ("build", vec![]),
        ("test", vec!["build"]),
        ("lint", vec![]),
        ("docs", vec!["lint"]),
        ("deploy", vec!["test"]),
    ]
    .into_iter()
    .map(|(j, n)| (j.to_string(), n.into_iter().map(String::from).collect()))
    .collect();
    let states = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(j, s)| (j.to_string(), s.to_string()))
            .collect()
    };

    // A failed job runs again with everything it needs, the rest that passed is kept
    let failed = states(&[
        ("build", "success"),
        ("test", "failure"),
        ("lint", "success"),
        ("docs", "success"),
        ("deploy", "not run, a job it needs failed"),
    ]);
    assert_eq!(keep_jobs(&graph, &failed), ["lint", "docs"]);
    let all = states(&[
        ("build", "success"),
        ("test", "success"),
        ("lint", "success"),
        ("docs", "success"),
        ("deploy", "success"),
    ]);
    assert_eq!(keep_jobs(&graph, &all).len(), 5);
    // A job the earlier result never had runs
    assert_eq!(
        keep_jobs(&graph, &states(&[("lint", "success")])),
        Vec::<String>::new()
    );
}

#[test]
fn an_id_names_the_one_key_it_starts() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("locks")).unwrap();
    fs::create_dir_all(home.path().join("cache")).unwrap();
    fs::write(home.path().join("locks/abcdef0123456789.lock"), "").unwrap();
    fs::write(home.path().join("cache/abcdef0123456789.json"), "").unwrap();
    fs::write(home.path().join("cache/abcdef9999999999.json"), "").unwrap();
    let find = |id: &str| find_key(home.path(), id);
    assert_eq!(find("abcdef0123"), Some("abcdef0123456789".into()));
    assert_eq!(find("abcdef"), None, "two trees start with it");
    assert_eq!(find("abcde"), None, "too short to name a tree");
    assert_eq!(find("feature"), None, "a branch is no id");
    assert_eq!(find("0000000000"), None);
}

#[test]
fn the_queue_gives_slots_in_order_and_skips_gone_runs() {
    let dir = tempfile::tempdir().unwrap();
    let queue = dir.path().join("queue");
    let slot = |t: &Ticket| match queue_step(dir.path(), t, 1).unwrap() {
        QueueStep::Slot(lock, _) => Ok(lock),
        QueueStep::Ahead(n) => Err(n),
    };
    // A child another test forks holds a dropped lock until it execs, so a slot can free a moment late
    let eventually = |t: &Ticket| {
        (0..100).find_map(|_| {
            let got = slot(t).ok();
            if got.is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
            got
        })
    };

    // A run the hooks started came first, `shipcheck run` still goes ahead of it
    let hook = Ticket::take(&queue, 1, &queue::Run::default()).unwrap();
    let agent = Ticket::take(&queue, 0, &queue::Run::default()).unwrap();
    assert_eq!(slot(&hook).err(), Some(1));
    let held = eventually(&agent).unwrap();
    drop(agent);
    // At the head, but the one slot is busy
    assert_eq!(slot(&hook).err(), Some(0));
    drop(held);

    // A ticket nobody holds is a run that died, and goes
    let gone = queue.join(format!("0-{:024}-1", 0));
    fs::write(&gone, "").unwrap();
    assert!(eventually(&hook).is_some());
    assert!(!gone.exists());
    let path = hook.path.clone();
    drop(hook);
    assert!(!path.exists());
}

#[test]
fn job_picks_host_checks() {
    let checks = BTreeMap::from([
        ("lint".to_string(), "make lint".into()),
        ("test".to_string(), "make test".into()),
    ]);
    assert_eq!(
        select_host(&checks, &["test".into()], &[]).unwrap(),
        BTreeMap::from([(HOST.to_string(), BTreeSet::from(["test".to_string()]))])
    );
    assert!(
        select_host(&checks, &["nope".into()], &[])
            .unwrap_err()
            .contains("the checks are: lint, test")
    );
    assert!(select_host(&checks, &[], &["ci.yml".into()]).is_err());
}

#[test]
fn last_run_finds_the_key_a_trial_base_gave_the_tree() {
    let home = tempfile::tempdir().unwrap();

    for (tree, key, base) in [
        ("t1", "main", "b1"),
        ("t1", "trial", "b2"),
        ("t2", "other", "b3"),
    ] {
        let line = serde_json::json!({"event": "run", "tree": tree, "key": key, "base": base});
        history::append(home.path(), "o/r", &line).unwrap();
    }

    assert_eq!(
        last_run(home.path(), "o/r", "t1", |_| true),
        Some(("trial".into(), "b2".into()))
    );
    assert_eq!(
        last_run(home.path(), "o/r", "t1", |k| k == "main"),
        Some(("main".into(), "b1".into()))
    );
    assert_eq!(last_run(home.path(), "o/r", "t9", |_| true), None);
}

#[test]
fn a_relative_home_is_made_absolute() {
    if std::env::var_os("SHIPCHECK_HOME_CHILD").is_some() {
        assert_eq!(home(), std::env::current_dir().unwrap().join("rel/home"));
        return;
    }

    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "run::tests::a_relative_home_is_made_absolute"])
        .env("SHIPCHECK_HOME_CHILD", "1")
        .env("SHIPCHECK_HOME", "rel/home")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn an_empty_home_variable_counts_as_unset() {
    if std::env::var_os("SHIPCHECK_HOME_CHILD").is_some() {
        assert_eq!(home(), PathBuf::from("/tmp/fake/.local/state/shipcheck"));
        return;
    }

    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "run::tests::an_empty_home_variable_counts_as_unset",
        ])
        .env("SHIPCHECK_HOME_CHILD", "1")
        .env("HOME", "/tmp/fake")
        .env("SHIPCHECK_HOME", "")
        .env("XDG_STATE_HOME", "")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn default_home_ignores_the_environment() {
    if std::env::var_os("SHIPCHECK_HOME_CHILD").is_some() {
        assert!(
            !default_home().starts_with("/tmp/fake"),
            "{}",
            default_home().display()
        );
        assert!(default_home().starts_with(user_dir()));
        assert!(home_overridden(), "HOME moved the results");
        return;
    }

    assert!(user_dir().is_absolute());
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "run::tests::default_home_ignores_the_environment",
        ])
        .env("SHIPCHECK_HOME_CHILD", "1")
        .env("HOME", "/tmp/fake")
        .env("XDG_STATE_HOME", "/tmp/fake/state")
        .env_remove("SHIPCHECK_HOME")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn safe_git_config_follows_the_users_own() {
    let vars: BTreeMap<_, _> = safe_git_env(2).into_iter().collect();
    assert_eq!(vars["GIT_CONFIG_COUNT"], "8");
    assert_eq!(vars["GIT_CONFIG_KEY_2"], "filter.lfs.process");
    assert_eq!(vars["GIT_CONFIG_KEY_7"], "core.fsmonitor");
    assert_eq!(vars["GIT_CONFIG_VALUE_6"], "false");
    assert!(!vars.contains_key("GIT_CONFIG_KEY_1"));
}

#[test]
fn secrets_longest_first_with_each_line() {
    let text = "# note\nA=abc\nB=\"abcdef\"\nC='x1\\ny2'\nnot a pair\nD=\n";
    assert_eq!(
        parse_secrets(text),
        ["abcdef", "x1\\ny2", "abc", "x1", "y2"]
    );
}

#[test]
fn skip_variable_values() {
    assert!(env_skips(Some("1")));
    assert!(env_skips(Some("yes")));
    assert!(!env_skips(None));
    assert!(!env_skips(Some("")));
    assert!(!env_skips(Some("0")));
    assert!(!env_skips(Some("False")));
}

#[test]
fn a_partial_run_picks_jobs_legs_workflows_and_their_needs() {
    let g = |jobs: &[(&str, &[&str])]| -> Vec<(String, Vec<String>)> {
        jobs.iter()
            .map(|(j, n)| {
                (
                    j.to_string(),
                    n.iter().map(std::string::ToString::to_string).collect(),
                )
            })
            .collect()
    };
    let graphs = BTreeMap::from([
        (
            "ci.yml".to_string(),
            g(&[
                ("lint", &[]),
                ("build", &[]),
                ("test__1", &["build"]),
                ("test__2", &["build"]),
                ("tests", &[]),
                ("deploy", &["test__1", "test__2"]),
            ]),
        ),
        ("docs.yml".to_string(), g(&[("docs", &[])])),
    ]);
    let s = |v: &[&str]| {
        v.iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
    };
    let set = |v: &[&str]| {
        v.iter()
            .map(std::string::ToString::to_string)
            .collect::<BTreeSet<_>>()
    };

    let picked = select(&graphs, &s(&["test"]), &[]).unwrap();
    assert_eq!(picked.len(), 1, "only the workflow with the job runs");
    assert_eq!(picked["ci.yml"], set(&["build", "test__1", "test__2"]));

    let picked = select(&graphs, &s(&["deploy"]), &[]).unwrap();
    assert_eq!(
        picked["ci.yml"],
        set(&["build", "deploy", "test__1", "test__2"])
    );

    let picked = select(&graphs, &s(&["lint"]), &s(&[".github/workflows/docs.yml"])).unwrap();
    assert_eq!(picked["ci.yml"], set(&["lint"]));
    assert_eq!(picked["docs.yml"], set(&["docs"]));

    // The refusal names each job as --job takes it
    assert_eq!(
        select(&graphs, &s(&["nope"]), &[]).unwrap_err(),
        "no job nope, the jobs are: build (ci.yml/build), deploy (ci.yml/deploy), lint (ci.yml/lint), test (ci.yml/test, legs test__1, test__2), tests (ci.yml/tests), docs (docs.yml/docs)"
    );
    assert!(select(&graphs, &s(&["test__"]), &[]).is_err());
    assert!(select(&graphs, &[], &s(&["release.yml"])).is_err());

    // `<file>/<job>` names one file's job only
    let both = BTreeMap::from([
        ("ci.yml".to_string(), g(&[("test", &[])])),
        (
            "e2e.yml".to_string(),
            g(&[("build", &[]), ("test", &["build"])]),
        ),
    ]);
    let picked = select(&both, &s(&["e2e.yml/test"]), &[]).unwrap();
    assert_eq!(picked.len(), 1);
    assert_eq!(picked["e2e.yml"], set(&["build", "test"]));
    assert_eq!(select(&both, &s(&["test"]), &[]).unwrap().len(), 2);
    assert!(select(&both, &s(&["docs.yml/test"]), &[]).is_err());
    assert!(select(&both, &s(&["ci.yml/build"]), &[]).is_err());
}

#[test]
fn a_config_places_each_job_on_one_runner() {
    let ci = "jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n  e2e:\n    needs: build\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";
    let set = |v: &[&str]| {
        v.iter()
            .map(std::string::ToString::to_string)
            .collect::<BTreeSet<_>>()
    };
    let jobs = |text: &str| {
        let cfg = RepoConfig::parse(text).unwrap();
        expected_jobs(&cfg, |f| (f == "ci.yml").then(|| ci.to_string()))
    };

    // An act job beside host checks brings the jobs it needs, and no other
    let mixed = jobs("runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n[jobs.\"ci.yml/e2e\"]\nrunner = \"act\"").unwrap();
    assert_eq!(mixed.len(), 2);
    assert_eq!(mixed[HOST], set(&["lint"]));
    assert_eq!(mixed["ci.yml"], set(&["build", "e2e"]));
    let names: Vec<String> = check::job_names(&mixed).into_iter().map(|n| n.2).collect();
    assert_eq!(
        names,
        ["shipcheck/build", "shipcheck/e2e", "shipcheck/lint"]
    );

    // Without [jobs], every job of a listed workflow, or only the host checks, as before
    let act = jobs("workflows = [\"ci.yml\"]").unwrap();
    assert_eq!(act.len(), 1);
    assert_eq!(act["ci.yml"], set(&["build", "e2e", "lint"]));
    let host = jobs("runner = \"host\"\n[host.checks]\nlint = \"make lint\"").unwrap();
    assert_eq!(host.len(), 1);
    assert_eq!(host[HOST], set(&["lint"]));

    // A host entry takes a job out of a listed workflow, and a host check of that name stands in
    let out = jobs("workflows = [\"ci.yml\"]\n[host.checks]\nlint = \"make lint\"\n[jobs.\"ci.yml/lint\"]\nrunner = \"host\"").unwrap();
    assert_eq!(out["ci.yml"], set(&["build", "e2e"]));
    let names: Vec<String> = check::job_names(&out).into_iter().map(|n| n.2).collect();
    assert_eq!(
        names,
        ["shipcheck/build", "shipcheck/e2e", "shipcheck/lint"]
    );
    let listed = "workflows = [\"ci.yml\"]\n";
    let entry = |id: &str| format!("[jobs.\"ci.yml/{id}\"]\nrunner = \"host\"\n");
    let checks = |names: &[&str]| {
        let lines: Vec<String> = names.iter().map(|n| format!("{n} = \"x\"\n")).collect();
        format!("[host.checks]\n{}", lines.concat())
    };
    let err = |text: String| jobs(&text).unwrap_err();

    assert!(
        err(format!("{listed}{}{}", checks(&["build"]), entry("build")))
            .contains("act job ci.yml/e2e needs build")
    );
    assert!(
        err(format!("{listed}{}{}", checks(&["lnit"]), entry("lnit")))
            .contains("\"ci.yml/lnit\" names no job of ci.yml")
    );
    assert!(err(format!("{listed}{}", entry("lint"))).contains("no check \"lint\""));
    assert!(
        err(format!("{listed}{}{}", checks(&["test"]), entry("lint")))
            .contains("no check \"lint\"")
    );
    // Both moved at once, whatever order the entries come in
    let both = jobs(&format!(
        "{listed}{}{}{}",
        checks(&["build", "e2e"]),
        entry("e2e"),
        entry("build")
    ))
    .unwrap();
    assert_eq!(both["ci.yml"], set(&["lint"]));
    assert_eq!(both[HOST], set(&["build", "e2e"]));
    let all = format!(
        "{listed}{}{}{}{}",
        checks(&["build", "e2e", "lint"]),
        entry("build"),
        entry("e2e"),
        entry("lint")
    );
    assert!(err(all).contains("every job of ci.yml is on the host runner"));
    assert!(
        jobs(
            "runner = \"host\"\n[host.checks]\nt = \"x\"\n[jobs.\"ci.yml/nope\"]\nrunner = \"act\""
        )
        .is_err()
    );
    // A host check and an act job of one name would post under one name, so the config fails
    let clash = jobs("runner = \"host\"\n[host.checks]\nbuild = \"cargo build\"\n[jobs.\"ci.yml/build\"]\nrunner = \"act\"").unwrap_err();
    assert_eq!(
        clash,
        format!(
            "host check `build` and job `ci.yml/build` would both post as shipcheck/build: rename one, for example the host check to `build-{}`",
            std::env::consts::OS
        )
    );
    // One a job of e2e needs counts too
    assert!(jobs("runner = \"host\"\n[host.checks]\nbuild = \"x\"\n[jobs.\"ci.yml/e2e\"]\nrunner = \"act\"").is_err());
}

#[test]
fn a_host_entry_takes_every_leg_of_a_job() {
    let g = |jobs: &[(&str, &[&str])]| -> Vec<(String, Vec<String>)> {
        jobs.iter()
            .map(|(j, n)| {
                (
                    j.to_string(),
                    n.iter().map(std::string::ToString::to_string).collect(),
                )
            })
            .collect()
    };
    let set = |v: &[&str]| {
        v.iter()
            .map(std::string::ToString::to_string)
            .collect::<BTreeSet<_>>()
    };
    let listed = "workflows = [\"ci.yml\"]\n";
    let entry = |id: &str| format!("[jobs.\"ci.yml/{id}\"]\nrunner = \"host\"\n");
    let checks = |names: &[&str]| {
        let lines: Vec<String> = names.iter().map(|n| format!("{n} = \"x\"\n")).collect();
        format!("[host.checks]\n{}", lines.concat())
    };
    let graphs = BTreeMap::from([(
        "ci.yml".to_string(),
        g(&[("lint__1", &[]), ("lint__2", &[]), ("test", &[])]),
    )]);
    let cfg =
        RepoConfig::parse(&format!("{listed}{}{}", checks(&["lint"]), entry("lint"))).unwrap();
    assert_eq!(
        act_jobs(&cfg, &graphs).unwrap()["ci.yml"],
        set(&["test"]),
        "a host entry takes every leg"
    );
}

#[test]
fn slots_are_exclusive_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let (first, lock) = claim_slot(dir.path()).unwrap();
    let (second, _keep) = claim_slot(dir.path()).unwrap();
    assert_ne!(first, second, "a held slot must not be handed out twice");
    drop(lock);
    // A child another test is spawning can hold the lock's descriptor until it execs
    let again = (0..50)
        .map(|_| {
            let (slot, _) = claim_slot(dir.path()).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            slot
        })
        .find(|slot| *slot == first)
        .unwrap_or_default();
    assert_eq!(
        again, first,
        "a freed slot must be reused, to keep warm paths stable"
    );
}

fn sh(dir: &Path, args: &[&str]) -> String {
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
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_checkout_passes_only_with_the_tested_tree_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("job");
    fs::create_dir(&repo).unwrap();
    sh(&repo, &["init", "-q"]);
    fs::write(repo.join("a"), "a").unwrap();
    fs::write(repo.join("e"), "e").unwrap();
    fs::set_permissions(repo.join("e"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("a", repo.join("l")).unwrap();
    fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "i"]);
    let tree = sh(&repo, &["rev-parse", "HEAD^{tree}"]);
    let restore = || sh(&repo, &["checkout", "-q", "--", "."]);

    // Build output, untracked files and new mtimes are not the code
    fs::create_dir(repo.join("target")).unwrap();
    fs::write(repo.join("target/stale"), "x").unwrap();
    fs::write(repo.join("untracked"), "x").unwrap();
    fs::File::open(repo.join("a"))
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH)
        .unwrap();
    verify_checkout(&repo, &tree).unwrap();
    assert!(!repo.with_extension("verify-index").exists());

    let tampers: [(&str, &dyn Fn()); 4] = [
        ("edited", &|| fs::write(repo.join("a"), "b").unwrap()),
        ("deleted", &|| fs::remove_file(repo.join("a")).unwrap()),
        ("mode", &|| {
            fs::set_permissions(repo.join("e"), fs::Permissions::from_mode(0o644)).unwrap();
        }),
        ("relinked", &|| {
            fs::remove_file(repo.join("l")).unwrap();
            symlink("e", repo.join("l")).unwrap();
        }),
    ];
    for (name, tamper) in tampers {
        tamper();
        assert!(verify_checkout(&repo, &tree).is_err(), "{name} must fail");
        restore();
        verify_checkout(&repo, &tree).unwrap();
    }

    let other = sh(&repo, &["rev-parse", "HEAD^{tree}"]).replace(char::is_alphanumeric, "0");
    // A wrong tree is the check's failure, exit 1, never "could not run"
    let e = verify_checkout(&repo, &other).unwrap_err();
    assert_eq!(error_state(&e), WRONG_TREE, "{e}");
}

#[test]
fn a_wrong_tree_fails_the_check_and_is_never_kept() {
    let entry: Entry = serde_json::from_value(serde_json::json!({
        "key": "k", "sha": "s", "tree": "t", "base": "origin/main", "passed": false,
        "finished_unix": 0,
        "workflows": [{
            "file": HOST, "exit": 1, "attempts": 1, "log_sha256": "", "passed": false,
            "jobs": {"test": WRONG_TREE, "fmt": "success"},
        }],
    }))
    .unwrap();
    assert!(entry.wrong_tree() && !entry.could_not_run());
    assert_eq!(exit_code(&entry), 1);
}

#[test]
fn asks_the_remote_for_its_default_branch_once_per_process() {
    let dir = tempfile::tempdir().unwrap();
    let (origin, repo) = (dir.path().join("origin.git"), dir.path().join("repo"));
    sh(
        dir.path(),
        &["init", "-q", "--bare", "-b", "trunk", "origin.git"],
    );
    sh(dir.path(), &["init", "-q", "-b", "trunk", "repo"]);
    sh(&repo, &["commit", "-q", "--allow-empty", "-m", "trunk"]);
    sh(
        &repo,
        &["remote", "add", "origin", &origin.to_string_lossy()],
    );
    sh(&repo, &["push", "-q", "origin", "trunk"]);
    // Each ls-remote of a local remote starts its upload-pack, which counts itself
    let count = dir.path().join("asked");
    let pack = dir.path().join("upload-pack");
    let script = format!(
        "#!/bin/sh\necho >> {}\nexec git upload-pack \"$@\"\n",
        count.display()
    );
    fs::write(&pack, script).unwrap();
    fs::set_permissions(&pack, fs::Permissions::from_mode(0o755)).unwrap();
    sh(
        &repo,
        &[
            "config",
            "remote.origin.uploadpack",
            &pack.to_string_lossy(),
        ],
    );

    for _ in 0..3 {
        assert_eq!(remote_base(&repo).as_deref(), Some("origin/trunk"));
    }

    assert_eq!(fs::read_to_string(&count).unwrap().lines().count(), 1);
}

#[test]
fn commit_and_post_refuse_the_default_branch() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    sh(repo, &["init", "-q", "-b", "main"]);
    sh(repo, &["commit", "-q", "--allow-empty", "-m", "main"]);
    let main = sh(repo, &["rev-parse", "HEAD"]);
    let main_branch = Some("main".to_string());
    assert_eq!(on_default_branch(repo, "origin/main", None), main_branch);
    assert_eq!(
        on_default_branch(repo, "origin/main", Some(&main)),
        main_branch
    );

    sh(repo, &["switch", "-q", "-c", "feature"]);
    sh(repo, &["commit", "-q", "--allow-empty", "-m", "feature"]);
    let feature = sh(repo, &["rev-parse", "HEAD"]);
    assert_eq!(on_default_branch(repo, "origin/main", None), None);
    assert_eq!(on_default_branch(repo, "origin/main", Some(&feature)), None);

    // A PR's commit posts from anywhere, main's tip from nowhere
    sh(repo, &["switch", "-q", "--detach", &feature]);
    assert_eq!(on_default_branch(repo, "origin/main", Some(&feature)), None);
    assert_eq!(
        on_default_branch(repo, "origin/main", Some(&main)),
        main_branch
    );
    sh(repo, &["switch", "-q", "main"]);
    assert_eq!(on_default_branch(repo, "origin/main", Some(&feature)), None);

    // The remote's tip counts too, and `base` names the branch
    sh(repo, &["update-ref", "refs/remotes/origin/trunk", &feature]);
    assert_eq!(
        on_default_branch(repo, "origin/trunk", Some(&feature)),
        Some("trunk".into())
    );
}

#[test]
fn removes_planted_links_and_keeps_their_targets() {
    let dir = tempfile::tempdir().unwrap();
    let (host, checkout) = (dir.path().join("host"), dir.path().join("checkout"));
    fs::create_dir_all(host.join("workflows")).unwrap();
    fs::write(host.join("zshrc"), "mine").unwrap();
    fs::create_dir_all(&checkout).unwrap();
    std::os::unix::fs::symlink(&host, checkout.join(".github")).unwrap();
    std::os::unix::fs::symlink(host.join("zshrc"), checkout.join(CONFIG)).unwrap();

    remove_planted_links(&checkout).unwrap();
    assert!(!checkout.join(".github").exists());
    assert!(!checkout.join(CONFIG).exists());
    assert_eq!(fs::read_to_string(host.join("zshrc")).unwrap(), "mine");
    assert!(host.join("workflows").is_dir());
}

#[test]
fn a_shallow_repository_gives_the_job_a_history_it_can_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, clone, job) = (
        tmp.path().join("repo"),
        tmp.path().join("clone"),
        tmp.path().join("job"),
    );
    fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);

    for n in ["a", "b", "c"] {
        sh(&repo, &["commit", "-q", "--allow-empty", "-m", n]);
    }

    let url = format!("file://{}", repo.display());
    let clone_s = clone.to_string_lossy().into_owned();
    sh(tmp.path(), &["clone", "-q", "--depth", "2", &url, &clone_s]);
    let sha = sh(&clone, &["rev-parse", "HEAD"]);
    let job_s = job.to_string_lossy().into_owned();
    sh(
        &clone,
        &["worktree", "add", "-q", "--detach", &job_s, "HEAD"],
    );

    standalone_git(&job, &sha).unwrap();
    assert_eq!(sh(&job, &["rev-list", "--count", "HEAD"]), "2");
}

#[test]
fn what_a_job_writes_in_its_checkout_runs_nothing_on_the_host() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, host) = (tmp.path().join("repo"), tmp.path().join("host"));
    let (template, job) = (tmp.path().join("template"), tmp.path().join("job"));
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(host.join("dir")).unwrap();
    fs::write(host.join("secret"), "mine").unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a").unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-qm", "a"]);
    let sha = sh(&repo, &["rev-parse", "HEAD"]);

    for dir in [&template, &job] {
        let dir = dir.to_string_lossy();
        sh(&repo, &["worktree", "add", "-q", "--detach", &dir, "HEAD"]);
    }

    let link = standalone_git(&job, &sha).unwrap();

    // The job: git config that names commands, hooks, a filter, and links out of the checkout
    let ran = |what: &str| tmp.path().join(format!("ran-{what}"));
    let touch = |what: &str| format!("touch '{}'", ran(what).display());
    let config = format!(
        "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n\tpager = {}\n[filter \"x\"]\n\tclean = {}\n\tsmudge = {}\n\tprocess = {}\n[diff \"x\"]\n\ttextconv = {}\n",
        touch("fsmonitor"),
        job.join(".git/hooks").display(),
        touch("pager"),
        touch("clean"),
        touch("smudge"),
        touch("process"),
        touch("textconv"),
    );
    let mut text = fs::read_to_string(job.join(".git/config")).unwrap();
    text.push_str(&config);
    fs::write(job.join(".git/config"), text).unwrap();
    fs::create_dir_all(job.join(".git/hooks")).unwrap();

    for hook in [
        "post-checkout",
        "post-index-change",
        "reference-transaction",
        "pre-auto-gc",
    ] {
        let path = job.join(".git/hooks").join(hook);
        fs::write(&path, format!("#!/bin/sh\n{}\n", touch("hook"))).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fs::write(job.join(".gitattributes"), "* filter=x diff=x\n").unwrap();
    fs::write(job.join(".gitignore"), "target\nout\n").unwrap();
    fs::create_dir_all(job.join("target")).unwrap();
    symlink(host.join("secret"), job.join("target/secret")).unwrap();
    symlink(host.join("dir"), job.join("target/dir")).unwrap();
    symlink(&host, job.join("out")).unwrap();
    fs::hard_link(job.join("a.txt"), job.join("target/hard")).unwrap();

    finish_job(&job, &template, link.as_deref(), &|_| {});

    let left: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ran-"))
        .collect();
    assert!(
        left.is_empty(),
        "the job's checkout ran {left:?} on the host"
    );
    assert_eq!(fs::read_to_string(host.join("secret")).unwrap(), "mine");
    assert!(host.join("dir").is_dir());
    assert_eq!(fs::read_dir(&job).unwrap().count(), 0);
    // Warming still works, through the host's worktree link. Linux temp dirs often cannot clone
    if cfg!(target_os = "macos") {
        assert!(
            template.join("target").is_dir(),
            "the template was not warmed"
        );
    }
}

#[test]
fn git_on_a_job_checkout_never_reads_its_git() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, job) = (tmp.path().join("repo"), tmp.path().join("job"));
    fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a").unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-qm", "a"]);
    sh(&repo, &["tag", "v1"]);
    let job_s = job.to_string_lossy();
    sh(
        &repo,
        &["worktree", "add", "-q", "--detach", &job_s, "HEAD"],
    );
    let link = fs::read_to_string(job.join(".git")).unwrap();
    let gitdir = PathBuf::from(link.trim().strip_prefix("gitdir: ").unwrap());

    // A job swapped `.git` for its own, and even the host's config names a monitor
    let ran = tmp.path().join("ran");
    let touch = format!("[core]\n\tfsmonitor = touch '{}'\n", ran.display());
    fs::remove_file(job.join(".git")).unwrap();
    fs::create_dir_all(job.join(".git")).unwrap();
    sh(&job, &["init", "-q"]);
    let own = fs::read_to_string(job.join(".git/config")).unwrap();
    fs::write(job.join(".git/config"), format!("{own}{touch}")).unwrap();
    let host = fs::read_to_string(repo.join(".git/config")).unwrap();
    fs::write(repo.join(".git/config"), format!("{host}{touch}")).unwrap();

    let tags = job_git(&job, &gitdir, &["tag", "--merged", "HEAD"]).unwrap();
    assert_eq!(tags, "v1", "git read the job's repository");
    job_git(&job, &gitdir, &["status", "--porcelain"]).unwrap();
    assert!(!ran.exists(), "git ran a monitor on the host");
}

#[test]
fn snapshots_the_working_copy_and_the_index_and_changes_neither() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    sh(repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a").unwrap();
    fs::write(repo.join("b.txt"), "b").unwrap();
    fs::write(repo.join(".gitignore"), "junk\n").unwrap();
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "main"]);
    assert_eq!(
        snapshot(repo, Tree::Working).unwrap(),
        None,
        "clean means HEAD"
    );
    assert_eq!(snapshot(repo, Tree::Staged).unwrap(), None);

    fs::write(repo.join("a.txt"), "stashed").unwrap();
    sh(repo, &["stash", "-q"]);
    fs::write(repo.join("a.txt"), "a2").unwrap();
    sh(repo, &["add", "a.txt"]);
    fs::write(repo.join("a.txt"), "a3").unwrap();
    fs::remove_file(repo.join("b.txt")).unwrap();
    fs::write(repo.join("new.txt"), "n").unwrap();
    fs::write(repo.join("junk"), "j").unwrap();
    let state = || {
        (
            sh(repo, &["--no-optional-locks", "status", "--porcelain"]),
            fs::read(repo.join(".git/index")).unwrap(),
            sh(repo, &["for-each-ref"]),
            sh(repo, &["stash", "list"]),
        )
    };
    let before = state();
    assert_eq!(untracked(repo).unwrap(), ["new.txt"], "junk is ignored");

    let wc = snapshot(repo, Tree::Working).unwrap().unwrap();
    let files = sh(repo, &["ls-tree", "-r", "--name-only", &wc]);
    assert_eq!(files, ".gitignore\na.txt\nnew.txt");
    assert_eq!(sh(repo, &["show", &format!("{wc}:a.txt")]), "a3");
    assert_eq!(
        sh(repo, &["rev-parse", &format!("{wc}^")]),
        sh(repo, &["rev-parse", "HEAD"])
    );

    let staged = snapshot(repo, Tree::Staged).unwrap().unwrap();
    let files = sh(repo, &["ls-tree", "-r", "--name-only", &staged]);
    assert_eq!(files, ".gitignore\na.txt\nb.txt");
    assert_eq!(sh(repo, &["show", &format!("{staged}:a.txt")]), "a2");

    assert!(
        before == state(),
        "the index, refs and stash must not change"
    );
    let left: Vec<_> = fs::read_dir(repo.join(".git"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("shipcheck-index")
        })
        .collect();
    assert!(left.is_empty(), "the temporary index must go");

    // Committing all of it gives the tree that ran, so the result is found by tree
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "all"]);
    assert_eq!(
        sh(repo, &["rev-parse", "HEAD^{tree}"]),
        sh(repo, &["rev-parse", &format!("{wc}^{{tree}}")])
    );
    assert_eq!(snapshot(repo, Tree::Working).unwrap(), None);
}

#[test]
fn the_index_copy_keeps_the_time_the_index_was_written() {
    let dir = tempfile::tempdir().unwrap();
    let (index, temp) = (dir.path().join("index"), dir.path().join("copy"));
    fs::write(&index, "entries").unwrap();
    let written = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs::File::options()
        .write(true)
        .open(&index)
        .unwrap()
        .set_modified(written)
        .unwrap();
    copy_index(&index, &temp).unwrap();
    assert_eq!(fs::read(&temp).unwrap(), b"entries");
    assert_eq!(fs::metadata(&temp).unwrap().modified().unwrap(), written);
}

#[test]
fn commit_runs_the_users_hooks_with_the_callers_path() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    // A tool in a git checkout, as ~/.nvm is, which the trusted PATH drops
    let tools = dir.path().join("nvm/bin");
    fs::create_dir_all(dir.path().join("nvm/.git")).unwrap();
    fs::create_dir_all(&tools).unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::write(tools.join("husky-tool"), "#!/bin/sh\ntouch \"$1\"\n").unwrap();
    // A fake git on the caller's PATH, which the commit itself must never run
    let faked = dir.path().join("fake-git-ran");
    let fake = format!("#!/bin/sh\ntouch {}\nexit 1\n", faked.display());
    fs::write(tools.join("git"), fake).unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(tools.join("husky-tool"))
        .arg(tools.join("git"))
        .status()
        .unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["config", "user.name", "t"]);
    sh(&repo, &["config", "user.email", "t@t"]);
    sh(&repo, &["config", "commit.gpgsign", "false"]);
    sh(&repo, &["commit", "-q", "--allow-empty", "-m", "main"]);
    // git init makes hooks/ only when it finds its templates
    fs::create_dir_all(repo.join(".git/hooks")).unwrap();
    let hook = repo.join(".git/hooks/pre-commit");
    let ran = dir.path().join("tool-ran");
    fs::write(&hook, format!("#!/bin/sh\nhusky-tool {}\n", ran.display())).unwrap();
    Command::new("chmod").arg("+x").arg(&hook).status().unwrap();
    fs::write(repo.join("a.txt"), "a").unwrap();
    let wc = snapshot(&repo, Tree::Working).unwrap().unwrap();
    let tested = Tested {
        tree: sh(&repo, &["rev-parse", &format!("{wc}^{{tree}}")]),
        staged: false,
        passed: true,
        partial: false,
    };
    let path = std::env::join_paths(
        std::iter::once(tools).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    CALLER_PATH.set(path).unwrap();

    commit(&repo, &tested, &["-m".into(), "a".into()], Failed::Refuse).unwrap();
    assert!(ran.exists(), "the hook found its tool on the caller's PATH");
    assert!(!faked.exists(), "git came from the trusted PATH");
}

#[test]
fn commit_never_signs() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    sh(repo, &["init", "-q", "-b", "main"]);
    sh(repo, &["config", "user.name", "t"]);
    sh(repo, &["config", "user.email", "t@t"]);
    sh(repo, &["commit", "-q", "--allow-empty", "-m", "main"]);
    sh(repo, &["config", "commit.gpgsign", "true"]);
    // A signer that always fails, as a locked key agent does
    sh(repo, &["config", "gpg.program", "false"]);
    fs::write(repo.join("a.txt"), "a").unwrap();
    let wc = snapshot(repo, Tree::Working).unwrap().unwrap();
    let tested = Tested {
        tree: sh(repo, &["rev-parse", &format!("{wc}^{{tree}}")]),
        staged: false,
        passed: true,
        partial: false,
    };

    commit(repo, &tested, &["-m".into(), "a".into()], Failed::Refuse).unwrap();
}

#[test]
fn commits_only_the_tree_that_ran() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    sh(repo, &["init", "-q", "-b", "main"]);
    sh(repo, &["config", "user.name", "t"]);
    sh(repo, &["config", "user.email", "t@t"]);
    sh(repo, &["config", "commit.gpgsign", "false"]);
    fs::write(repo.join("a.txt"), "a").unwrap();
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "main"]);
    // git init makes hooks/ only when it finds its templates
    fs::create_dir_all(repo.join(".git/hooks")).unwrap();
    let hook = repo.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        "#!/bin/sh\ntouch \"$(git rev-parse --git-dir)/hook-ran\"\n",
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&hook).status().unwrap();

    fs::write(repo.join("a.txt"), "a2").unwrap();
    sh(repo, &["add", "a.txt"]);
    fs::write(repo.join("a.txt"), "a3").unwrap();
    fs::write(repo.join("b.txt"), "b").unwrap();
    let wc = snapshot(repo, Tree::Working).unwrap().unwrap();
    let tested = Tested {
        tree: sh(repo, &["rev-parse", &format!("{wc}^{{tree}}")]),
        staged: false,
        passed: true,
        partial: false,
    };
    let msg = ["-m".to_string(), "all".to_string()];
    let head = sh(repo, &["rev-parse", "HEAD"]);
    let index = || fs::read(repo.join(".git/index")).unwrap();
    let before = index();

    let failed = Tested {
        passed: false,
        ..tested.clone()
    };
    assert!(
        commit(repo, &failed, &msg, Failed::Refuse)
            .unwrap_err()
            .contains("failed")
    );

    // A partial run that passed is still not the check, and the refusal says so
    let partial = Tested {
        partial: true,
        ..tested.clone()
    };
    let err = commit(repo, &partial, &msg, Failed::Allow).unwrap_err();
    assert!(err.starts_with("the last run was partial"), "{err}");

    fs::write(repo.join("c.txt"), "after the run").unwrap();
    let err = commit(repo, &tested, &msg, Failed::Refuse).unwrap_err();
    assert!(err.contains("files changed since the run"), "{err}");
    assert_eq!(sh(repo, &["rev-parse", "HEAD"]), head);
    assert!(index() == before, "a refusal keeps the staged state");
    fs::remove_file(repo.join("c.txt")).unwrap();

    let sha = commit(repo, &tested, &msg, Failed::Refuse).unwrap();
    assert_eq!(sh(repo, &["rev-parse", "HEAD"]), sha);
    assert_eq!(sh(repo, &["rev-parse", "HEAD^{tree}"]), tested.tree);
    assert_eq!(sh(repo, &["rev-parse", "HEAD^"]), head);
    assert_eq!(sh(repo, &["log", "-1", "--format=%s"]), "all");
    assert_eq!(
        sh(repo, &["status", "--porcelain"]),
        "",
        "the index follows"
    );
    assert!(repo.join(".git/hook-ran").exists(), "git's own hooks run");

    // With --staged, only the index goes in
    fs::write(repo.join("a.txt"), "a4").unwrap();
    sh(repo, &["add", "a.txt"]);
    fs::write(repo.join("d.txt"), "left out").unwrap();
    let staged = snapshot(repo, Tree::Staged).unwrap().unwrap();
    let tested = Tested {
        tree: sh(repo, &["rev-parse", &format!("{staged}^{{tree}}")]),
        staged: true,
        passed: true,
        partial: false,
    };
    commit(repo, &tested, &msg, Failed::Refuse).unwrap();
    assert_eq!(sh(repo, &["rev-parse", "HEAD^{tree}"]), tested.tree);
    assert_eq!(sh(repo, &["status", "--porcelain"]), "?? d.txt");
}

#[test]
fn names_ci_changes_since_the_branch_left_the_base() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let ci = |coe: &str, extra: &str| {
        format!(
            "on: push\njobs:\n  test:{coe}\n    runs-on: x\n    steps: [{{run: make}}]\n{extra}"
        )
    };
    let write = |body: String| {
        fs::create_dir_all(repo.join(WORKFLOWS)).unwrap();
        fs::write(repo.join(WORKFLOWS).join("ci.yml"), body).unwrap();
        sh(repo, &["add", "-A"]);
    };
    sh(repo, &["init", "-q", "-b", "main"]);
    write(ci("", ""));
    sh(repo, &["commit", "-qm", "main"]);
    sh(repo, &["checkout", "-q", "-b", "pr"]);
    write(ci("\n    continue-on-error: true", ""));
    fs::write(repo.join(CONFIG), "workflows = []").unwrap();
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "weaken"]);
    let sha = sh(repo, &["rev-parse", "HEAD"]);
    // main moves on with a job of its own, which the branch must not be blamed for removing
    sh(repo, &["checkout", "-q", "main"]);
    write(ci("", "  lint: {runs-on: x, steps: [{run: lint}]}\n"));
    sh(repo, &["commit", "-qm", "lint"]);

    assert_eq!(
        ci_changes(repo, "main", &sha).unwrap(),
        [
            "`.github/workflows/ci.yml` changes",
            "`.github/workflows/ci.yml`: job `test` gets continue-on-error",
            "`shipcheck.toml` is added",
        ]
    );
    assert_eq!(
        ci_changes(repo, "main", "main").unwrap(),
        Vec::<std::string::String>::new()
    );
}

#[test]
fn names_changes_to_what_commands_run() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let pkg = |test: &str, dep: &str| {
        format!("{{\"scripts\": {{\"test\": \"{test}\"}}, \"dependencies\": {{\"a\": \"{dep}\"}}}}")
    };
    sh(repo, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(repo.join("web")).unwrap();
    fs::write(repo.join("package.json"), pkg("jest", "1")).unwrap();
    fs::write(repo.join("web/package.json"), pkg("jest", "1")).unwrap();
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "main"]);
    sh(repo, &["checkout", "-q", "-b", "pr"]);
    fs::write(repo.join("package.json"), pkg("jest", "2")).unwrap();
    fs::write(repo.join("web/package.json"), pkg("true", "1")).unwrap();
    fs::write(repo.join("Makefile"), "test:\n\ttrue\n").unwrap();
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "cheat"]);
    let sha = sh(repo, &["rev-parse", "HEAD"]);

    assert_eq!(
        ci_changes(repo, "main", &sha).unwrap(),
        [
            "`Makefile` is added, it can change what checks run",
            "`web/package.json` changes what checks may run",
        ]
    );
}

/// A repository with `src/lib.rs` and `docs/a.md` on main, and a plan for the commit
/// `change` makes on top of it
#[test]
fn failed_needs_keep_passed_on_main() {
    let (_dir, plan) = plan_after(|repo| fs::write(repo.join("src/lib.rs"), "fn b() {}").unwrap());
    let err = run_only_failed(&plan, &mut |_| {}).unwrap_err();
    assert!(err.contains("keep_passed = true"), "{err}");
}

fn plan_after(change: impl Fn(&Path)) -> (tempfile::TempDir, Plan) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    sh(repo, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("src/lib.rs"), "fn a() {}").unwrap();
    fs::write(repo.join("docs/a.md"), "a").unwrap();
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "main"]);
    let base = sh(repo, &["rev-parse", "HEAD"]);
    change(repo);
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "change"]);
    let sha = sh(repo, &["rev-parse", "HEAD"]);
    let config_text = b"workflows = [\"ci.yml\"]\npaths = [\"src/**\"]\n".to_vec();
    let plan = Plan {
        repo: repo.to_path_buf(),
        tree: sh(repo, &["rev-parse", "HEAD^{tree}"]),
        sha,
        base,
        config: RepoConfig::parse(&String::from_utf8_lossy(&config_text)).unwrap(),
        config_text,
        workflows: BTreeMap::new(),
        key: "k".into(),
        ci_changes: Vec::new(),
        tools: String::new(),
        branch: None,
        inputs: String::new(),
    };
    (dir, plan)
}

#[test]
fn a_stored_result_counts_only_when_what_it_shows_holds() {
    if std::env::var_os("SHIPCHECK_HOME_CHILD").is_none() {
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run::tests::a_stored_result_counts_only_when_what_it_shows_holds",
            ])
            .env("SHIPCHECK_HOME_CHILD", "1")
            .env("SHIPCHECK_HOME", home.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        return;
    }

    let (_d, plan) = plan_after(|r| fs::write(r.join("src/lib.rs"), "fn b() {}").unwrap());
    let store = |tree: &str, base: &str, log: &str| {
        let entry = serde_json::json!({
            "key": plan.key, "sha": plan.sha, "tree": tree, "base": base,
            "passed": true, "finished_unix": 0,
            "workflows": [{"file": HOST, "exit": 0, "attempts": 1, "jobs": {"t": "success"},
                "passed": true, "log_sha256": cache::hex(&Sha256::digest(b"the log"))}]
        });
        fs::create_dir_all(home().join("cache")).unwrap();
        fs::write(
            home().join(format!("cache/{}.json", plan.key)),
            entry.to_string(),
        )
        .unwrap();
        fs::create_dir_all(log_dir(&plan.key)).unwrap();
        fs::write(log_dir(&plan.key).join(format!("{HOST}.log")), log).unwrap();
    };
    store(&plan.tree, &plan.base, "the log");
    assert!(lookup(&plan).is_some());
    store(&"0".repeat(40), &plan.base, "the log");
    assert!(
        lookup(&plan).is_none(),
        "a result of another tree under this key"
    );
    store(&plan.tree, &plan.sha, "the log");
    assert!(
        lookup(&plan).is_none(),
        "a base that is not on the plan's base"
    );
    store(&plan.tree, &plan.base, "another log");
    assert!(
        lookup(&plan).is_none(),
        "a log that does not hash to its digest"
    );
    fs::remove_file(log_dir(&plan.key).join(format!("{HOST}.log"))).unwrap();
    assert!(lookup(&plan).is_none(), "a missing log");
}

#[test]
fn paths_skip_only_when_nothing_under_paths_changed() {
    let (_d, docs_only) = plan_after(|r| fs::write(r.join("docs/a.md"), "b").unwrap());
    assert!(skipped(&docs_only).is_some());

    let (_d, edited) = plan_after(|r| fs::write(r.join("src/lib.rs"), "fn b() {}").unwrap());
    assert!(skipped(&edited).is_none());

    let (_d, deleted) = plan_after(|r| fs::remove_file(r.join("src/lib.rs")).unwrap());
    assert!(
        skipped(&deleted).is_none(),
        "deleting a file under paths must run"
    );

    let (_d, moved_out) = plan_after(|r| {
        fs::rename(r.join("src/lib.rs"), r.join("docs/lib.rs")).unwrap();
    });
    assert!(
        skipped(&moved_out).is_none(),
        "moving a file out of paths must run"
    );

    let (_d, unicode) = plan_after(|r| fs::write(r.join("src/café é.rs"), "x").unwrap());
    assert!(
        skipped(&unicode).is_none(),
        "a quoted name must still match paths"
    );
}

#[test]
fn escapes_control_characters_for_the_terminal() {
    let out = printable("ok\ttab\n\x1b]0;title\x07\x1b[2J\rback\u{9b}31m\x7f");
    assert_eq!(
        out,
        "ok\ttab\n\\u{1b}]0;title\\u{7}\\u{1b}[2J\\rback\\u{9b}31m\\u{7f}"
    );
    assert!(
        !out.chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    );
}

#[test]
fn a_job_waits_out_the_engine_view_of_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let slot = dir.path().join("0");
    let stamp = ended_stamp(&slot, &slot.join("ci.yml/test"));
    assert_eq!(stamp, dir.path().join("0.ended/ci.yml/test"));
    assert_eq!(ended_stamp(&slot, &slot), dir.path().join("0.ended/slot"));

    let now = SystemTime::now();
    assert_eq!(engine_wait(&stamp, now), Duration::ZERO, "no job ran there");
    mark_ended(&stamp);
    let wait = engine_wait(&stamp, SystemTime::now());
    assert!(
        wait > Duration::from_millis(1500) && wait <= ENGINE_VIEW,
        "{wait:?}"
    );
    let later = SystemTime::now() + Duration::from_secs(3);
    assert_eq!(engine_wait(&stamp, later), Duration::ZERO);
    let earlier = SystemTime::now() - Duration::from_secs(60);
    assert_eq!(engine_wait(&stamp, earlier), ENGINE_VIEW);
    // A new run deletes the slot, the stamps beside it stay
    fs::create_dir_all(slot.join("ci.yml/test")).unwrap();
    fs::remove_dir_all(&slot).unwrap();
    assert!(stamp.exists());
}

#[test]
fn a_failed_attempt_keeps_its_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("test.log");
    fs::write(&log, "attempt 1").unwrap();
    let kept = keep_attempt_log(&log, 1).unwrap();
    assert_eq!(kept, dir.path().join("test.attempt-1.log"));
    fs::write(&log, "attempt 2").unwrap();
    assert_eq!(fs::read_to_string(kept).unwrap(), "attempt 1");
}

#[test]
fn skip_markers_follow_github() {
    assert!(message_skips("wip [skip ci]"));
    assert!(message_skips("Docs only\n\n[CI SKIP]"));
    assert!(message_skips("[skip shipcheck] try something"));
    assert!(!message_skips("skip ci when docs change"));
    assert!(!message_skips("fix the skip list"));
}

#[test]
fn prune_removes_only_old_results() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("cache")).unwrap();
    fs::create_dir_all(home.path().join("logs/oldkey")).unwrap();
    fs::create_dir_all(home.path().join("logs/monthkey")).unwrap();
    fs::write(home.path().join("cache/old.json"), "{}").unwrap();
    fs::write(home.path().join("cache/new.json"), "{}").unwrap();
    let days = |n: u64| SystemTime::now() - Duration::from_secs(n * 24 * 3600);

    for (p, age) in [
        ("cache/old.json", 30),
        ("logs/monthkey", 30),
        ("logs/oldkey", 100),
    ] {
        fs::File::open(home.path().join(p))
            .unwrap()
            .set_modified(days(age))
            .unwrap();
    }

    prune(home.path(), SystemTime::now());
    assert!(!home.path().join("cache/old.json").exists());
    assert!(!home.path().join("logs/oldkey").exists());
    assert!(
        home.path().join("logs/monthkey").exists(),
        "logs stay 90 days"
    );
    assert!(home.path().join("cache/new.json").exists());
}

type Marks<'a> = &'a [(&'a str, u32, u32, bool)];

fn marks(of: Marks<'_>) -> BTreeMap<String, workflow::JobMarks> {
    of.iter()
        .map(|&(job, started, ended, failed)| {
            let m = workflow::JobMarks {
                started,
                ended,
                failed,
            };
            (job.to_string(), m)
        })
        .collect()
}

#[test]
fn job_state_from_the_act_exit_and_the_markers() {
    let (ok, cut, bad) = (
        ("test", 1, 1, false),
        ("test", 1, 0, false),
        ("test", 1, 1, true),
    );
    let no_image = "no image for its runs-on label on this host";
    let cases: &[(Option<i32>, bool, Marks<'_>, bool, &str)] = &[
        (Some(0), false, &[ok], false, "success"),
        (Some(0), false, &[("test", 2, 2, false)], false, "success"),
        // A job whose `if:` is false prints no markers
        (Some(0), false, &[], false, "skipped"),
        (Some(0), false, &[cut], false, "killed"),
        (Some(0), false, &[("test", 2, 1, false)], false, "killed"),
        (Some(0), false, &[bad], false, "failure"),
        (Some(0), false, &[("other", 1, 1, false)], false, "failure"),
        (
            Some(0),
            false,
            &[ok, ("other", 1, 0, false)],
            false,
            "failure",
        ),
        (Some(1), false, &[ok], false, "failure"),
        (Some(1), false, &[], false, "failure"),
        (Some(1), false, &[cut], false, "killed"),
        (None, false, &[ok], false, "timed out"),
        (Some(0), true, &[ok], false, no_image),
        (Some(0), false, &[ok], true, "success"),
        (Some(0), false, &[], true, "skipped"),
        (
            Some(1),
            false,
            &[bad],
            true,
            "failure, allowed by continue-on-error",
        ),
        (Some(0), true, &[], true, no_image),
    ];

    for (exit, unsupported, of, allow, want) in cases {
        let act = if *unsupported {
            ActRun::NoImage
        } else if of.is_empty() {
            ActRun::Skipped
        } else {
            ActRun::Ran
        };
        let on_error = if *allow {
            OnError::Continue
        } else {
            OnError::Fail
        };
        assert_eq!(
            job_state("test", *exit, act, &marks(of), on_error),
            *want,
            "exit {exit:?}, unsupported {unsupported}, marks {of:?}, allowed {allow}"
        );
    }

    // A job act ran none of, with no word that its `if:` skipped it, is not skipped
    assert_eq!(
        job_state("test", Some(0), ActRun::Ran, &marks(&[]), OnError::Fail),
        "failure, act ran nothing"
    );
}

#[test]
fn a_skip_stands_only_when_the_if_is_false_on_every_pr() {
    let job = |id: &str, cond: &str| {
        format!(
            "  {id}:\n    if: {cond}\n    runs-on: ubuntu-latest\n    steps:\n      - run: make\n"
        )
    };
    let gate = "(github.ref == 'refs/heads/main' || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)";
    let wf = [
            "on: [push, pull_request, workflow_dispatch]\njobs:\n  label:\n    if: contains(github.event.pull_request.labels.*.name, 'slow')\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        os: [a, b]\n    steps:\n      - run: make slow\n".to_string(),
            job("dispatch", "github.event_name != 'workflow_dispatch'"),
            job("vars", "vars.RUN_E2E == 'true'"),
            job("either", "github.event_name == 'push' || github.event_name == 'pull_request'"),
            job("not_tag", "${{ !startsWith(github.ref, 'refs/tags/') }}"),
            job("push", "github.event_name == 'push'"),
            job("release", "github.event_name == 'push' && github.ref == format('refs/heads/{0}', github.event.repository.default_branch)"),
            job("tag", "startsWith(github.ref, 'refs/tags/')"),
            job("off", "false"),
            job("off_expr", "${{ false }}"),
            job("gated_push", &format!("(github.event_name == 'push') && {gate}")),
            job("gated_vars", &format!("(vars.RUN_E2E == 'true') && {gate}")),
        ]
        .concat();
    let skip_on_prs = skipped_on_every_pr(&job_ifs(&wf));
    let state = |job: &str| {
        act_skip(
            job_state(job, Some(0), ActRun::Skipped, &marks(&[]), OnError::Fail),
            job,
            &skip_on_prs,
        )
    };

    // GitHub runs these on a PR, or may, so act's skip says nothing
    for job in [
        "label",
        "label__2",
        "dispatch",
        "vars",
        "either",
        "not_tag",
        "gated_vars",
    ] {
        assert_eq!(state(job), IF_SKIP, "{job}");
        let states = BTreeMap::from([(job.to_string(), state(job))]);
        assert!(!workflow_passed(&states), "{job}: {states:?}");
    }

    // GitHub skips these on a PR too
    for job in ["push", "release", "tag", "off", "off_expr", "gated_push"] {
        assert_eq!(state(job), "skipped", "{job}");
    }

    assert_eq!(
        act_skip("success".into(), "vars", &skip_on_prs),
        "success",
        "a job act ran stands"
    );
}

#[test]
fn continue_on_error_never_allows_a_timeout() {
    // AGENTS.md: a timeout is a failure, and this deadline is shipcheck's own
    let timed_out = job_state("test", None, ActRun::Ran, &marks(&[]), OnError::Continue);
    let states = BTreeMap::from([
        ("lint".to_string(), "success".to_string()),
        ("test".to_string(), timed_out.clone()),
    ]);
    assert!(
        !workflow_passed(&states),
        "a workflow passed with a timed out job: {states:?}"
    );
    assert_eq!(
        timed_out, "timed out",
        "continue-on-error allowed a timeout"
    );
}

#[test]
fn continue_on_error_never_allows_a_killed_job() {
    // A job that started and never printed its end marker did not end, so no step failed
    let killed = job_state(
        "test",
        Some(1),
        ActRun::Ran,
        &marks(&[("test", 1, 0, false)]),
        OnError::Continue,
    );
    let states = BTreeMap::from([
        ("lint".to_string(), "success".to_string()),
        ("test".to_string(), killed.clone()),
    ]);
    assert!(
        !workflow_passed(&states),
        "a workflow passed with a killed job: {states:?}"
    );
    assert_eq!(killed, "killed", "continue-on-error allowed a killed job");
}

#[test]
fn workflow_passes_only_when_every_job_passed_and_one_succeeded_or_was_skipped() {
    let allowed = format!("failure{ALLOWED}");
    let cases: &[(&[&str], bool)] = &[
        (&[], false),
        (&["success"], true),
        (&["success", "success"], true),
        (&["success", "skipped"], true),
        (&["success", &allowed], true),
        (&["skipped"], true),
        (&["skipped", "skipped"], true),
        (&[&allowed], false),
        (&["skipped", &allowed], true),
        (&["skipped", "failure, act ran nothing"], false),
        (&["success", "failure"], false),
        (&["success", "killed"], false),
        (&["success", "timed out"], false),
        (
            &["success", "no image for its runs-on label on this host"],
            false,
        ),
        (&["success", "not run, a job it needs did not pass"], false),
        (&["success", "not run, its needs form a cycle"], false),
        (&["success", "failure, shipcheck could not run it"], false),
    ];

    for (jobs, want) in cases {
        let states = jobs
            .iter()
            .enumerate()
            .map(|(n, s)| (format!("j{n}"), s.to_string()))
            .collect();
        assert_eq!(workflow_passed(&states), *want, "{jobs:?}");
    }
}

#[test]
fn plan_reads_the_config_and_the_workflows_from_main() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let ci = format!("{WORKFLOWS}/ci.yml");
    let main_config = "runner = \"host\"\n[host]\nchecks = { test = \"true\" }\n";
    let main_ci =
        "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";
    sh(repo, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(repo.join(WORKFLOWS)).unwrap();
    fs::write(repo.join(CONFIG), main_config).unwrap();
    fs::write(repo.join(&ci), main_ci).unwrap();
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "main"]);

    // The PR edits both files and adds a workflow, and the working copy edits them again
    sh(repo, &["switch", "-q", "-c", "feature"]);
    fs::write(
        repo.join(CONFIG),
        main_config.replace("\"true\"", "\"exit 0\""),
    )
    .unwrap();
    fs::write(repo.join(&ci), main_ci.replace("run: true", "run: exit 0")).unwrap();
    fs::write(repo.join(WORKFLOWS).join("new.yml"), main_ci).unwrap();
    sh(repo, &["add", "-A"]);
    sh(repo, &["commit", "-qm", "feature"]);
    fs::write(repo.join(CONFIG), "runner = \"act\"\n").unwrap();
    fs::write(repo.join(&ci), "on: push\n").unwrap();

    let plan = plan(repo, "HEAD", "main").unwrap();
    assert_eq!(plan.sha, sh(repo, &["rev-parse", "feature"]));
    assert_eq!(String::from_utf8_lossy(&plan.config_text), main_config);
    assert_eq!(plan.config.host.checks["test"], "true");
    assert_eq!(
        plan.workflows,
        BTreeMap::from([(ci, main_ci.as_bytes().to_vec())])
    );
}
