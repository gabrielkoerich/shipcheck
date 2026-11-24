use super::*;
use crate::runner;
use crate::runner::tests;
use std::os::unix::fs::PermissionsExt;
use yaml_rust2::YamlLoader;

fn load(s: &str) -> Yaml {
    YamlLoader::load_from_str(s).unwrap().remove(0)
}

fn inject_with(src: &str, nonce: &str, act: config::Act) -> Result<String, String> {
    workflow::add_markers(&Act::new(act).prepare(src, "ci.yml")?, nonce)
}

fn inject(src: &str, nonce: &str) -> Result<String, String> {
    inject_with(src, nonce, config::Act::default())
}

const CI: &str = "\
name: CI
on:
  push:
    branches: [main]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          cargo build
          cargo test
";

#[test]
fn adds_dispatch_to_every_trigger_form() {
    for on in [
        "on: push",
        "on: [push, pull_request]",
        "on:\n  push:\n    branches: [main]",
    ] {
        let out = load(&inject(&format!("{on}\njobs: {{}}\n"), "n").unwrap());
        let on = &out["on"];
        let has = match on {
            Yaml::Hash(h) => h.contains_key(&key("workflow_dispatch")),
            Yaml::Array(a) => a.contains(&key("workflow_dispatch")),
            _ => false,
        };
        assert!(has, "no workflow_dispatch in {on:?}");
    }
}

#[test]
fn the_checkout_step_needs_both_nonces() {
    let script = nonce_step("abc")["run"].as_str().unwrap().to_string();
    let run = |dir: &Path| {
        let out = Command::new(runner::posix_sh())
            .args(["-e", "-c", &script])
            .env("GITHUB_WORKSPACE", dir)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let dir = tempfile::tempdir().unwrap();
    let git = dir.path().join(".git");
    fs::create_dir_all(&git).unwrap();

    let (ok, out) = run(dir.path());
    assert!(!ok && out.contains("it sees no checkout"), "{out}");

    fs::write(git.join(NONCE_FILE), "old").unwrap();
    let (ok, out) = run(dir.path());
    assert!(!ok && out.contains("it sees an older checkout"), "{out}");

    fs::write(git.join(NONCE_FILE), "abc").unwrap();
    let (ok, out) = run(dir.path());
    assert!(
        !ok && out.contains("an older checkout"),
        "no root nonce: {out}"
    );

    write_nonces(dir.path(), "abc").unwrap();
    assert_eq!(run(dir.path()), (true, String::new()));
}

#[test]
fn the_root_nonce_replaces_a_planted_link_and_stays_out_of_git() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::write(outside.path().join("zshrc"), "mine").unwrap();
    std::os::unix::fs::symlink(outside.path().join("zshrc"), dir.path().join(ROOT_NONCE)).unwrap();
    write_nonces(dir.path(), "abc").unwrap();
    assert_eq!(
        fs::read_to_string(outside.path().join("zshrc")).unwrap(),
        "mine"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(ROOT_NONCE)).unwrap(),
        "abc"
    );
    let exclude = fs::read_to_string(dir.path().join(".git/info/exclude")).unwrap();
    assert!(exclude.contains("/.shipcheck-nonce\n"));
}

#[test]
fn shards_pytest_only_when_the_config_asks() {
    let wf = "on: push\njobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n      - run: uv run pytest\n";
    let off = Act::default().prepare(wf, "ci.yml").unwrap();
    assert!(
        !off.contains(shard::MARK),
        "a config without shard_tests must keep its key"
    );
    let on = Act::default()
        .with_shards(config::ShardTests::Fixed(3))
        .prepare(wf, "ci.yml")
        .unwrap();
    let step = &load(&on)["jobs"]["t"]["steps"][1];
    assert_eq!(
        step["name"].as_str(),
        Some("Run uv run pytest (shipcheck: 3 shards)")
    );
    assert!(
        !on.contains("nproc"),
        "the shard count must stay out of the key"
    );
}

#[test]
fn runs_the_jobs_init_gated_to_the_base_branch() {
    let wf = "on:\n  push:\n    branches: [main, \"v[0-9]+\"]\n  pull_request:\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make\n  b:\n    runs-on: ubuntu-latest\n    if: always()\n    steps:\n      - run: make\n";
    let gated = workflow::gate_jobs(wf, &["a".into(), "b".into()], "main").unwrap();
    let out = load(&Act::default().prepare(&gated, "ci.yml").unwrap());
    assert!(matches!(out["jobs"]["a"]["if"], Yaml::BadValue), "{out:?}");
    assert_eq!(out["jobs"]["b"]["if"].as_str(), Some("always()"));
}

#[test]
fn guards_a_step_whose_if_a_pr_may_answer_otherwise() {
    let wf = "on: [push, pull_request]\njobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n      - id: o\n        run: echo x=1 >> \"$GITHUB_OUTPUT\"\n      - name: pr\n        if: github.event_name == 'pull_request'\n        run: make test\n      - if: \"${{ always() && vars.E2E == 'true' }}\"\n        run: make e2e\n      - if: \"${{ !cancelled() && github.head_ref != '' }}\"\n        run: make lint\n      - if: github.ref == 'refs/heads/main'\n        run: make deploy\n      - if: failure() && github.event_name == 'pull_request'\n        run: make report\n      - if: steps.o.outputs.x == '1' && matrix.os != 'windows'\n        run: make more\n";
    let out = load(&Act::default().prepare(wf, "ci.yml").unwrap());
    let steps = out["jobs"]["t"]["steps"].as_vec().unwrap();
    let ifs: Vec<(&str, &str)> = steps
        .iter()
        .map(|s| {
            (
                s["name"].as_str().or(s["run"].as_str()).unwrap_or(""),
                s["if"].as_str().unwrap_or(""),
            )
        })
        .collect();
    let guard = |c: &str| ("shipcheck if", format!("${{{{ success() && !({c}) }}}}"));
    let at = |name: &str| ifs.iter().position(|(n, _)| n.starts_with(name)).unwrap();

    for (step, cond) in [
        ("pr", "github.event_name == 'pull_request'"),
        ("make e2e", "true && vars.E2E == 'true'"),
        ("make lint", "true && github.head_ref != ''"),
    ] {
        let g = &ifs[at(step) - 1];
        assert_eq!((g.0, g.1.to_string()), guard(cond), "{ifs:?}");
        assert!(matches!(steps[at(step) - 1]["id"], Yaml::BadValue));
        assert_eq!(steps[at(step) - 1]["env"]["STEP"].as_str(), Some(step));
    }

    // GitHub skips the deploy on every PR, runs the report only on a failure, and the last
    // step reads only what act knows
    for step in ["make deploy", "make report", "make more"] {
        assert_ne!(ifs[at(step) - 1].0, "shipcheck if", "{ifs:?}");
    }

    assert_eq!(ifs.iter().filter(|(n, _)| *n == "shipcheck if").count(), 3);
    assert_eq!(steps[at("echo x=1")]["id"].as_str(), Some("o"));

    // An env value at any level that reads the PR makes the step's `if:` read it, a static one
    // does not, and a step's own static value hides nothing
    let wf = "on: push\nenv:\n  TOP: ${{ github.event_name == 'pull_request' }}\njobs:\n  t:\n    runs-on: ubuntu-latest\n    env:\n      IS_PR: ${{ github.event_name == 'pull_request' }}\n      X: 'false'\n      SHADOW: ${{ github.event_name == 'pull_request' }}\n    steps:\n      - if: env.IS_PR == 'true'\n        run: make pr\n      - if: env.TOP == 'true'\n        run: make top\n      - if: env.X == 'true'\n        run: make static\n      - if: env.SHADOW == 'true'\n        env:\n          SHADOW: false\n        run: make shadowed\n";
    let out = load(&Act::default().prepare(wf, "ci.yml").unwrap());
    let steps = out["jobs"]["t"]["steps"].as_vec().unwrap();
    let before = |run: &str| {
        let at = steps
            .iter()
            .position(|s| s["run"].as_str() == Some(run))
            .unwrap();
        steps[at - 1]["name"].as_str() == Some("shipcheck if")
    };
    assert!(before("make pr"), "{steps:?}");
    assert!(before("make top"), "{steps:?}");
    assert!(!before("make static"), "{steps:?}");
    assert!(before("make shadowed"), "{steps:?}");

    // The workflow as it runs, which the cache key holds, carries the guards
    assert_eq!(inject(wf, "n").unwrap().matches("shipcheck if").count(), 3);
}

#[test]
fn keeps_existing_triggers() {
    let out = load(&inject(CI, "n").unwrap());
    assert_eq!(out["on"]["push"]["branches"][0].as_str(), Some("main"));
}

#[test]
fn setup_uv_keeps_its_cache_in_the_checkout_only() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let setup = dir.path().join(".github/actions/setup");
    fs::create_dir_all(&setup).unwrap();
    let composite = "runs:\n  using: composite\n  steps:\n    - uses: astral-sh/setup-uv@v7\n      with:\n        enable-cache: true\n";
    fs::write(setup.join("action.yml"), composite).unwrap();
    fs::write(outside.path().join("action.yml"), composite).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join(".github/actions/linked")).unwrap();
    let wf = "on: push\njobs:\n  t:\n    steps:\n      - uses: astral-sh/setup-uv@v6\n      - uses: ./.github/actions/setup\n      - uses: ./.github/actions/linked\n      - uses: ./../escape\n";

    let out = load(&keep_caches(dir.path(), wf).unwrap());
    assert_eq!(
        out["jobs"]["t"]["steps"][0]["with"]["prune-cache"].as_str(),
        Some("false")
    );
    let action = load(&fs::read_to_string(setup.join("action.yml")).unwrap());
    let with = &action["runs"]["steps"][0]["with"];
    assert_eq!(with["prune-cache"].as_str(), Some("false"));
    assert_eq!(with["enable-cache"].as_bool(), Some(true));
    assert_eq!(
        fs::read_to_string(outside.path().join("action.yml")).unwrap(),
        composite
    );

    let plain = "on: push\njobs:\n  t:\n    steps:\n      - run: true\n";
    assert_eq!(keep_caches(dir.path(), plain).unwrap(), plain);

    // An action.yml with aliases is left as the branch wrote it, never expanded
    let bomb = "a: &a [x, x]\nb: &b [*a, *a]\nruns:\n  using: composite\n  steps:\n    - uses: astral-sh/setup-uv@v7\n";
    fs::write(setup.join("action.yml"), bomb).unwrap();
    keep_caches(dir.path(), wf).unwrap();
    assert_eq!(fs::read_to_string(setup.join("action.yml")).unwrap(), bomb);
}

#[test]
fn pnpm_installs_move_out_of_the_setup_step() {
    let dir = tempfile::tempdir().unwrap();
    let setup = dir.path().join("setup");
    fs::create_dir_all(&setup).unwrap();
    let composite = "runs:\n  using: composite\n  steps:\n    - uses: pnpm/action-setup@v4\n      with:\n        run_install: true\n";
    fs::write(setup.join("action.yml"), composite).unwrap();
    let bomb = dir.path().join("bomb");
    fs::create_dir_all(&bomb).unwrap();
    let aliased = "a: &a [x]\nruns:\n  using: composite\n  steps:\n    - uses: pnpm/action-setup@v4\n      with: {run_install: true, b: *a}\n";
    fs::write(bomb.join("action.yml"), aliased).unwrap();
    let wf = r#"on: push
jobs:
  t:
    steps:
      - uses: pnpm/action-setup@v4
        if: always()
        env: {CI: "1"}
        with:
          version: 10
          run_install: |
            - args: [--frozen-lockfile]
              recursive: true
            - cwd: web
      - uses: pnpm/action-setup@v4
        with: {run_install: "${{ matrix.install }}"}
      - uses: pnpm/action-setup@v4
        with: {run_install: false}
      - uses: ./setup
      - uses: ./bomb
"#;
    let out = load(&keep_caches(dir.path(), wf).unwrap());
    let steps = out["jobs"]["t"]["steps"].as_vec().unwrap();
    assert_eq!(steps.len(), 7);
    assert!(steps[0]["with"]["run_install"].is_badvalue());
    assert_eq!(steps[0]["with"]["version"].as_i64(), Some(10));
    assert_eq!(
        steps[1]["run"].as_str(),
        Some("pnpm recursive install --frozen-lockfile")
    );
    assert_eq!(steps[1]["if"].as_str(), Some("always()"));
    assert_eq!(steps[1]["env"]["CI"].as_str(), Some("1"));
    assert_eq!(steps[2]["run"].as_str(), Some("pnpm install"));
    assert_eq!(steps[2]["working-directory"].as_str(), Some("web"));
    assert_eq!(
        steps[3]["with"]["run_install"].as_str(),
        Some("${{ matrix.install }}")
    );
    assert_eq!(steps[4]["with"]["run_install"].as_bool(), Some(false));

    let action = load(&fs::read_to_string(setup.join("action.yml")).unwrap());
    let inner = action["runs"]["steps"].as_vec().unwrap();
    assert!(inner[0]["with"]["run_install"].is_badvalue());
    assert_eq!(inner[1]["run"].as_str(), Some("pnpm recursive install"));
    assert_eq!(inner[1]["shell"].as_str(), Some("bash"));
    assert_eq!(
        fs::read_to_string(bomb.join("action.yml")).unwrap(),
        aliased
    );
}

#[test]
fn the_homes_step_owns_the_dirs_docker_made_above_a_mount() {
    let out = load(&inject(CI, "abc").unwrap());
    let dir = tempfile::tempdir().unwrap();
    let (home, bin) = (dir.path().join("home"), dir.path().join("bin"));
    let homes = out["jobs"]["test"]["steps"][1]["run"]
        .as_str()
        .unwrap()
        .replace("/home/runner", &home.to_string_lossy());
    // As Docker leaves a volume at ~/.bun/install/cache: the dirs above it are root's
    fs::create_dir_all(home.join(".bun/install/cache")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("sudo"), "#!/bin/sh\nchmod u+w \"$4\"\n").unwrap();
    fs::set_permissions(bin.join("sudo"), fs::Permissions::from_mode(0o755)).unwrap();
    for d in [".bun/install", ".bun"] {
        fs::set_permissions(home.join(d), fs::Permissions::from_mode(0o555)).unwrap();
    }

    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let status = std::process::Command::new("bash")
        .args(["-e", "-c", &homes])
        .env("PATH", path)
        .env("HOME", &home)
        .env("GITHUB_WORKSPACE", dir.path())
        .env("RUNNER_TEMP", dir.path().join("temp"))
        .env("CARGO_HOME", dir.path().join("cargo"))
        .env("GITHUB_PATH", dir.path().join("github_path"))
        .status()
        .unwrap();
    assert!(status.success());
    assert!(fs::create_dir(home.join(".bun/install/bin")).is_ok());
    assert!(fs::create_dir(home.join(".bun/bin")).is_ok());
}

#[test]
fn a_jobs_entry_picks_the_engine_of_its_job_and_legs() {
    let act = Act::default().with_engines(BTreeMap::from([(
        "ci.yml/e2e".to_string(),
        config::Engine::Colima,
    )]));
    assert_eq!(act.engine("ci.yml", "e2e").name(), "colima");
    assert_eq!(act.engine("ci.yml", "e2e__2").name(), "colima");
    assert_eq!(act.engine("ci.yml", "test").name(), "docker");
    assert_eq!(act.engine("other.yml", "e2e").name(), "docker");
    assert!(act.uses_colima() && !Act::default().uses_colima());
}

/// Removes a volume a test made, also when one of its assertions fails
struct OwnVolume(String);

impl Drop for OwnVolume {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["volume", "rm", &self.0])
            .output();
    }
}

/// Every volume name on the machine without `part`
fn volumes_without(part: &str) -> BTreeSet<String> {
    let out = Command::new("docker")
        .args(["volume", "ls", "-q"])
        .output()
        .unwrap();
    (String::from_utf8_lossy(&out.stdout).split_whitespace())
        .filter(|n| !n.contains(part))
        .map(String::from)
        .collect()
}

/**
 * shipcheck stands in a child copy of this test: it starts the reaper and a fake act, then
 * waits for the signal. Ctrl-C reaches shipcheck's group only, act and the reaper have their
 * own, so the reaper has to clean up from the end of its input alone
 **/
#[test]
#[ignore = "needs Docker"]
fn a_signal_to_shipcheck_makes_the_reaper_remove_acts_volumes() {
    if let Some(dir) = std::env::var_os("SHIPCHECK_REAPER_CHILD") {
        let mut reaper = reaper(Path::new(&dir), None).unwrap();
        let act = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        writeln!(reaper.stdin.as_mut().unwrap(), "{}", act.id()).unwrap();
        println!("ready");
        std::io::stdout().flush().unwrap();
        // The signal ends this process first
        let _ = act.wait_with_output();
        let _ = reaper.wait();
        return;
    }

    for signal in [libc::SIGINT, libc::SIGTERM] {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir(&run_dir).unwrap();
        let tag = run_tag(&run_dir);
        let before = volumes_without(&tag);
        let name = format!("act-CI-test-{tag}-0abc-env");
        let made = Command::new("docker")
            .args(["volume", "create", &name])
            .output()
            .unwrap();
        let _own = OwnVolume(name.clone());
        assert!(made.status.success());

        let mut shipcheck = Command::new(std::env::current_exe().unwrap())
            .args([
                "runner::act::tests::a_signal_to_shipcheck_makes_the_reaper_remove_acts_volumes",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("SHIPCHECK_REAPER_CHILD", &run_dir)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = std::io::BufReader::new(shipcheck.stdout.take().unwrap());
        let mut line = String::new();

        while !line.contains("ready") {
            line.clear();
            assert!(std::io::BufRead::read_line(&mut out, &mut line).unwrap() > 0);
        }

        let pid = libc::pid_t::try_from(shipcheck.id()).expect("a pid fits a pid_t");
        // SAFETY: kill only sends a signal to the child this test started
        unsafe { libc::kill(pid, signal) };
        let status = shipcheck.wait().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(signal)
        );
        let deadline = Instant::now() + Duration::from_secs(60);

        let exists = || {
            Command::new("docker")
                .args(["volume", "inspect", &name])
                .output()
                .is_ok_and(|o| o.status.success())
        };

        while exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }

        assert!(!exists(), "signal {signal} left {name}");
        assert_eq!(
            volumes_without(&tag),
            before,
            "the test changed volumes it did not make"
        );
    }
}

#[test]
fn the_reaper_cleans_up_only_when_shipcheck_dies_first() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("docker.log");
    let docker = dir.path().join("docker");
    // `ps` names one container, `volume ls` an act volume, one that is not and one whose tag is out of
    // place, as Docker's substring filter lets through. Every call is logged
    let fake = format!(
        "#!/bin/sh\necho \"$*\" >> {}\n[ \"$1\" = ps ] && echo c1\n[ \"$1\" = volume ] && [ \"$2\" = ls ] && printf 'act-CI-t-13f69a23-0a-env\\nact-x-13f69a23-0a\\nact-13f69a23-x-env\\n'\nexit 0\n",
        log.display()
    );
    fs::write(&docker, fake).unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", dir.path().display());
    // A slot lock at 10 or higher, which a shell cannot redirect, so it must come in lower
    let lock = tests::high(fs::File::create(dir.path().join("slot.lock")).unwrap());

    // Ended by itself, killed at the timeout, or shipcheck died
    for state in ["done", "exited", ""] {
        let _ = fs::remove_file(&log);
        let mut act = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let mut cmd = Command::new(runner::posix_sh());
        let held = super::super::inherit(&mut cmd, vec![std::os::fd::AsRawFd::as_raw_fd(&lock)]);
        let mut reaper = cmd
            .args(["-c", REAPER, "sh", "net-1", "13f69a23", "/runs/0"])
            .env("PATH", &path)
            .env("LOCK_FDS", held[0].to_string())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = reaper.stdin.take().unwrap();
        writeln!(stdin, "{}", act.id()).unwrap();

        if !state.is_empty() {
            writeln!(stdin, "{state}").unwrap();
        }

        drop(stdin);
        assert!(reaper.wait().unwrap().success());
        let calls = fs::read_to_string(&log).unwrap_or_default();
        let cleanup = "ps -aq --filter name=^act- --filter volume=/runs/0\nrm -f -v c1\nvolume ls -q --filter name=-13f69a23-\nvolume rm act-CI-t-13f69a23-0a-env\nnetwork rm net-1\n";

        match state {
            "done" => assert_eq!(calls, ""),
            _ => assert_eq!(calls, cleanup, "{state}"),
        }

        // After `exited` the group id may be another's, so only a dead shipcheck's reaper kills
        if state.is_empty() {
            assert!(act.wait().unwrap().code().is_none(), "act still runs");
        } else {
            assert!(
                act.try_wait().unwrap().is_none(),
                "{state} killed the group"
            );
            let _ = act.kill();
        }

        let _ = act.wait();
    }
}

#[test]
fn puts_homes_between_the_markers() {
    let out = load(&inject(CI, "abc").unwrap());
    let steps = out["jobs"]["test"]["steps"].as_vec().unwrap();
    assert_eq!(steps.len(), 5);
    assert_eq!(
        steps[0]["run"].as_str(),
        Some("echo \"shipcheck-start abc ${{ github.job }}\"")
    );
    let homes = steps[1]["run"].as_str().unwrap();
    assert!(homes.starts_with(
            "for d in \"/home/runner/.shipcheck\" \"/home/runner/.npm\" \"/home/runner/.bun/install/cache\" \"/home/runner/.cache\" \"$GITHUB_WORKSPACE/.venv\"; do\n  while [ -w \"$d\" ] || sudo -n chown"
        ));
    assert!(homes.ends_with(
            "done\nmkdir -p \"$RUNNER_TEMP\" || true\nmkdir -p \"$CARGO_HOME/bin\"\necho \"$CARGO_HOME/bin\" >> \"$GITHUB_PATH\"\n"
        ));
    assert_eq!(
        out["jobs"]["test"]["env"]["RUNNER_TEMP"].as_str(),
        Some("/home/runner/work/_temp")
    );
    assert_eq!(steps[2]["uses"].as_str(), Some("actions/checkout@v4"));
    assert_eq!(steps[3]["run"].as_str(), Some("cargo build\ncargo test\n"));
    assert_eq!(steps[4]["if"].as_str(), Some("always()"));
}

#[test]
fn gives_each_job_its_own_tool_homes() {
    let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - run: cargo build
  b:
    runs-on: ubuntu-latest
    env:
      CARGO_HOME: /custom
    steps:
      - run: cargo test
";
    let out = load(&inject(wf, "n").unwrap());
    assert_eq!(
        out["jobs"]["a"]["env"]["RUSTUP_HOME"].as_str(),
        Some("/home/runner/.shipcheck/a/rustup")
    );
    assert_eq!(
        out["jobs"]["a"]["env"]["CARGO_HOME"].as_str(),
        Some("/home/runner/.shipcheck/a/cargo")
    );
    // A job that sets its own home keeps it
    assert_eq!(
        out["jobs"]["b"]["env"]["CARGO_HOME"].as_str(),
        Some("/custom")
    );
    assert_eq!(
        out["jobs"]["b"]["env"]["RUSTUP_HOME"].as_str(),
        Some("/home/runner/.shipcheck/b/rustup")
    );
}

#[test]
fn takes_tool_homes_from_the_table() {
    let act = config::Act {
        job_homes: BTreeMap::from([("GOMODCACHE".to_string(), config::Home::Dir("gomod".into()))]),
        ..Default::default()
    };
    let out = load(&inject_with(CI, "n", act).unwrap());
    let env = &out["jobs"]["test"]["env"];
    assert_eq!(
        env["GOMODCACHE"].as_str(),
        Some("/home/runner/.shipcheck/test/gomod")
    );
    assert!(env["CARGO_HOME"].is_badvalue());
    // No home has a `path`, so the homes step only hands the mounts to the job's user
    let steps = out["jobs"]["test"]["steps"].as_vec().unwrap();
    assert_eq!(steps.len(), 5);
    assert!(
        steps[1]["run"]
            .as_str()
            .unwrap()
            .ends_with("done\nmkdir -p \"$RUNNER_TEMP\" || true\n")
    );
    assert_eq!(steps[2]["uses"].as_str(), Some("actions/checkout@v4"));
}

#[test]
fn an_empty_homes_table_adds_nothing() {
    let act = config::Act {
        job_homes: BTreeMap::new(),
        ..Default::default()
    };
    let out = load(&inject_with(CI, "n", act).unwrap());
    let env = out["jobs"]["test"]["env"].as_hash().unwrap();
    assert!(env.keys().all(|k| {
        let k = k.as_str().unwrap();
        k.starts_with("GIT_CONFIG_") || k == "RUNNER_TEMP"
    }));
    assert_eq!(out["jobs"]["test"]["steps"].as_vec().unwrap().len(), 5);
}

#[test]
fn container_jobs_and_mapped_labels_keep_their_own() {
    let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    container: node:22
    steps:
      - run: npm test
";
    let out = load(&inject(wf, "n").unwrap());
    assert!(out["jobs"]["a"]["env"]["RUNNER_TEMP"].is_badvalue());
    assert_eq!(out["jobs"]["a"]["runs-on"].as_str(), Some("ubuntu-24.04"));
    let act = config::Act {
        platforms: BTreeMap::from([("ubuntu-latest".into(), "img@sha256:1".into())]),
        ..Default::default()
    };
    let out = load(&inject_with(wf, "n", act).unwrap());
    assert_eq!(out["jobs"]["a"]["runs-on"].as_str(), Some("ubuntu-latest"));
}

#[test]
fn a_dry_run_says_when_an_if_skips_the_job() {
    // act 0.2.89 with --dryrun --verbose, for `if: github.event_name == 'pull_request'`
    let skipped = "*DRYRUN* [w.yml/x] [DEBUG] Skipping job 'x' due to 'github.event_name == 'pull_request''\n";
    assert!(dry_run_skips(skipped));
    let ran = "*DRYRUN* [w.yml/x] [DEBUG] evaluating expression 'success()'\n*DRYRUN* [w.yml/x] ⭐ Run Set up job\n";
    assert!(!dry_run_skips(ran));
}

#[test]
fn the_key_sees_the_default_images() {
    let mapped = config::Act {
        platforms: BTreeMap::from([("ubuntu-22.04".into(), "img@sha256:1".into())]),
        ..Default::default()
    };
    let p = platforms(&mapped);
    assert_eq!(p["ubuntu-22.04"], "img@sha256:1");
    assert!(p["ubuntu-24.04"].contains("27043bae70b3"));
    assert_eq!(p["ubuntu-latest"], p["ubuntu-24.04"]);

    // GitHub's other Linux labels, which act otherwise skips as unsupported
    let p = platforms(&config::Act::default());
    for (label, base) in [
        ("ubuntu-slim", "ubuntu-24.04"),
        ("ubuntu-24.04-arm", "ubuntu-24.04"),
        ("ubuntu-22.04-arm", "ubuntu-22.04"),
    ] {
        assert_eq!(p.get(label), Some(&p[base]), "{label}");
    }
}

#[test]
fn trusts_the_mounted_repository_unless_the_job_configures_git() {
    let out = load(&inject(CI, "n").unwrap());
    assert_eq!(
        out["jobs"]["test"]["env"]["GIT_CONFIG_KEY_0"].as_str(),
        Some("safe.directory")
    );
    let own = CI.replace("    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4", "    runs-on: ubuntu-latest\n    env: {GIT_CONFIG_COUNT: '0'}\n    steps:\n      - uses: actions/checkout@v4");
    let out = load(&inject(&own, "n").unwrap());
    assert_eq!(
        out["jobs"]["test"]["env"]["GIT_CONFIG_COUNT"].as_str(),
        Some("0")
    );
    assert!(out["jobs"]["test"]["env"]["GIT_CONFIG_KEY_0"].is_badvalue());
}

#[test]
fn finds_unsupported_platform_skips() {
    let log = "[ci/test__1] 🚧  Skipping unsupported platform -- Try running with `-P macos-latest=...`\n\
                   [ci/test__2] ⭐ Run Set up job\n";
    assert_eq!(unsupported_jobs(log), ["test__1"]);
}

fn combos(matrix: &str) -> Vec<String> {
    let m = load(matrix);
    legs(m.as_hash().unwrap())
        .unwrap()
        .iter()
        .map(|c| {
            c.iter()
                .map(|(k, v)| format!("{}={}", k.as_str().unwrap(), v.as_str().unwrap()))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

#[test]
fn matrix_axes_exclude_and_include_follow_github() {
    assert_eq!(
        combos("os: [a, b]\nv: ['1', '2']"),
        ["os=a,v=1", "os=a,v=2", "os=b,v=1", "os=b,v=2"]
    );
    assert_eq!(
        combos("os: [a, b]\nv: ['1', '2']\nexclude: [{os: b, v: '2'}]"),
        ["os=a,v=1", "os=a,v=2", "os=b,v=1"]
    );
    // extends the combinations it does not overwrite, else becomes a new one
    assert_eq!(
        combos("os: [a, b]\ninclude: [{os: a, x: y}, {os: c}, {extra: z}]"),
        ["os=a,x=y,extra=z", "os=b,extra=z", "os=c"]
    );
    assert_eq!(
        combos("include: [{os: a, fs: f1}, {os: b, fs: f2}]"),
        ["os=a,fs=f1", "os=b,fs=f2"]
    );
}

#[test]
fn expression_matrices_are_left_to_act() {
    assert!(legs(load("os: ${{ fromJSON(inputs.list) }}").as_hash().unwrap()).is_none());
}

#[test]
fn expands_matrix_jobs_and_rewrites_needs() {
    let wf = "\
on: push
jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        include:
          - {os: macos-latest, fs: apfs}
          - {os: ubuntu-latest, fs: ext4}
    runs-on: ${{ matrix.os }}
    steps:
      - run: echo ${{ matrix.fs }}
  release:
    needs: test
    runs-on: ubuntu-latest
    steps:
      - run: echo release
";
    let out = load(&inject(wf, "n").unwrap());
    let jobs = out["jobs"].as_hash().unwrap();
    let ids: Vec<&str> = jobs.keys().map(|k| k.as_str().unwrap()).collect();
    assert_eq!(ids, ["test__1", "test__2", "release"]);
    assert_eq!(
        out["jobs"]["test__1"]["runs-on"].as_str(),
        Some("macos-latest")
    );
    // act would give `ubuntu-latest` ImageOS ubuntu20
    assert_eq!(
        out["jobs"]["test__2"]["runs-on"].as_str(),
        Some("ubuntu-24.04")
    );
    assert_eq!(
        out["jobs"]["test__2"]["strategy"]["matrix"]["include"][0]["fs"].as_str(),
        Some("ext4")
    );
    assert_eq!(
        out["jobs"]["test__2"]["strategy"]["fail-fast"].as_bool(),
        Some(false)
    );
    assert_eq!(out["jobs"]["release"]["needs"][1].as_str(), Some("test__2"));
    assert_eq!(out["jobs"]["test__1"]["steps"].as_vec().unwrap().len(), 4);
}

#[test]
fn resolves_a_matrix_runs_on_with_a_fallback() {
    // pallets/flask's tests.yaml
    let wf = "\
on: push
jobs:
  tests:
    runs-on: ${{ matrix.os || 'ubuntu-latest' }}
    strategy:
      matrix:
        include:
          - {python: '3.14', os: windows-latest}
          - {python: '3.13'}
          - {python: '3.12', os: ''}
    steps: [{run: echo}]
";
    let out = load(&inject(wf, "n").unwrap());
    let runs_on = |j: &str| out["jobs"][j]["runs-on"].as_str().map(str::to_string);
    assert_eq!(runs_on("tests__1").as_deref(), Some("windows-latest"));
    assert_eq!(runs_on("tests__2").as_deref(), Some("ubuntu-24.04"));
    assert_eq!(runs_on("tests__3").as_deref(), Some("ubuntu-24.04"));

    let leg = load("{os: macos-latest}").as_hash().unwrap().clone();
    let resolve = |v: &str| resolve_matrix(&key(v), &leg);
    assert_eq!(resolve("${{ matrix.os }}"), Some(key("macos-latest")));
    assert_eq!(
        resolve("${{ matrix.arch || matrix.os }}"),
        Some(key("macos-latest"))
    );
    assert_eq!(resolve("${{ matrix.arch }}"), None);
    assert_eq!(resolve("${{ matrix.arch || inputs.os }}"), None);
    assert_eq!(resolve("${{ matrix.os && 'x' }}"), None);
}

#[test]
fn a_job_no_platform_maps_gets_no_network() {
    let labels: BTreeSet<String> = ["ubuntu-24.04".to_string()].into();
    let wf = "\
on: push
jobs:
  mac: {runs-on: macos-latest, steps: [{run: echo}]}
  list: {runs-on: [self-hosted, macos], steps: [{run: echo}]}
  linux: {runs-on: ubuntu-24.04, steps: [{run: echo}]}
  either: {runs-on: [macos-latest, ubuntu-24.04], steps: [{run: echo}]}
  expr: {runs-on: '${{ inputs.os }}', steps: [{run: echo}]}
  group: {runs-on: {group: big}, steps: [{run: echo}]}
";
    assert!(!runs_here(wf, "mac", &labels));
    assert!(!runs_here(wf, "list", &labels));
    assert!(runs_here(wf, "linux", &labels));
    assert!(runs_here(wf, "either", &labels));
    assert!(runs_here(wf, "expr", &labels));
    assert!(runs_here(wf, "group", &labels));
}

#[test]
fn a_network_waits_for_an_address_pool_until_its_deadline() {
    let full = "Error response from daemon: all predefined address pools have been fully subnetted";
    let pause = Duration::from_millis(1);
    let later = Instant::now() + Duration::from_secs(60);
    let mut calls = 0;
    let created = until_pooled(later, pause, || {
        calls += 1;
        if calls < 3 { Err(full.into()) } else { Ok(()) }
    });
    assert_eq!((created, calls), (Ok(()), 3));

    let mut calls = 0;
    let other = until_pooled(later, pause, || {
        calls += 1;
        Err("permission denied".into())
    });
    assert_eq!((other, calls), (Err("permission denied".into()), 1));

    let past = Instant::now();
    let mut calls = 0;
    let late = until_pooled(past, pause, || {
        calls += 1;
        Err(full.into())
    });
    assert_eq!((late, calls), (Err(full.into()), 1));
}

#[test]
fn a_job_that_failed_before_its_first_step_never_started() {
    let pulled = "[CI/a] ⭐ Run Set up job\n[CI/a]   🐳  docker pull image=catthehacker/ubuntu\n\
                      Error: context deadline exceeded\n";
    assert!(never_started(pulled));
    let ran = "[CI/a] ⭐ Run Main echo \"shipcheck-start n ${{ github.job }}\"\n\
                   [CI/a]   | shipcheck-start n a\n[CI/a]   | sh: cargo: not found\n";
    assert!(!never_started(ran));
    assert!(!invalid_workflow(pulled) && !invalid_workflow(ran));
    let parse = "level=info msg=\"Using docker host\"\n\nError: workflow is not valid. 'ci.yml': Line: 25 Column 1: Unknown Property cache-mode\n";
    assert!(invalid_workflow(parse) && never_started(parse));
}

#[test]
fn resolves_continue_on_error_per_leg() {
    let wf = "\
on: push
jobs:
  t:
    runs-on: ubuntu-latest
    continue-on-error: ${{ matrix.experimental }}
    strategy:
      matrix:
        php: ['8.3']
        experimental: [false]
        include: [{php: nightly, experimental: true}]
    steps: [{run: echo}]
";
    let out = inject(wf, "n").unwrap();
    let parsed = load(&out);
    assert_eq!(
        parsed["jobs"]["t__1"]["continue-on-error"].as_bool(),
        Some(false)
    );
    assert_eq!(
        parsed["jobs"]["t__2"]["continue-on-error"].as_bool(),
        Some(true)
    );
    let allowed = workflow::allowed_failures(&out).unwrap();
    assert_eq!(allowed.into_iter().collect::<Vec<_>>(), ["t__2"]);
}

#[test]
fn gives_listed_jobs_their_own_label() {
    let wf = "\
on: push
jobs:
  checks:
    runs-on: blacksmith-4vcpu-ubuntu-2404
    steps:
      - run: echo checks
  build:
    runs-on: blacksmith-4vcpu-ubuntu-2404
    strategy:
      matrix:
        part: [a, b]
    steps:
      - run: echo build
";
    let act = config::Act {
        jobs: BTreeMap::from([("ci.yml/build".to_string(), "img@sha256:amd64".to_string())]),
        ..Default::default()
    };
    let out = load(&inject_with(wf, "n", act).unwrap());
    assert_eq!(
        out["jobs"]["checks"]["runs-on"].as_str(),
        Some("blacksmith-4vcpu-ubuntu-2404")
    );
    assert_eq!(
        out["jobs"]["build__1"]["runs-on"].as_str(),
        Some("shipcheck-build")
    );
    assert_eq!(
        out["jobs"]["build__2"]["runs-on"].as_str(),
        Some("shipcheck-build")
    );
}

#[test]
fn resolves_anchors_and_flow_style() {
    let wf = "\
on: [push]
x-steps: &steps
  - run: echo shared
jobs: {a: {runs-on: ubuntu-latest, steps: *steps}, b: {runs-on: ubuntu-latest, steps: *steps}}
";
    let out = load(&inject(wf, "n").unwrap());
    assert!(
        out["on"]
            .as_vec()
            .unwrap()
            .contains(&key("workflow_dispatch"))
    );

    for job in ["a", "b"] {
        let steps = out["jobs"][job]["steps"].as_vec().unwrap();
        assert_eq!(steps.len(), 4, "{job}");
        assert_eq!(steps[2]["run"].as_str(), Some("echo shared"));
    }
}

#[test]
fn keeps_dispatch_once_and_its_inputs() {
    let wf = "\
on:
  workflow_dispatch:
    inputs:
      level: {default: low}
jobs: {}
";
    let out = load(&inject(wf, "n").unwrap());
    assert_eq!(
        out["on"]["workflow_dispatch"]["inputs"]["level"]["default"].as_str(),
        Some("low")
    );
}

#[test]
fn marks_empty_steps_and_leaves_array_runs_on() {
    let wf = "\
on: push
jobs:
  empty:
    runs-on: [self-hosted, linux]
    steps: []
";
    let out = load(&inject(wf, "n").unwrap());
    assert_eq!(out["jobs"]["empty"]["steps"].as_vec().unwrap().len(), 3);
    assert_eq!(
        out["jobs"]["empty"]["runs-on"][0].as_str(),
        Some("self-hosted")
    );
}

#[test]
fn rewrites_needs_through_chains_of_matrices() {
    let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    strategy: {matrix: {node: [18, 20]}}
    steps: [{run: echo a}]
  b:
    runs-on: ubuntu-latest
    needs: a
    steps: [{run: echo b}]
  c:
    runs-on: ubuntu-latest
    needs: [b, a]
    steps: [{run: echo c}]
";
    let out = load(&inject(wf, "n").unwrap());
    let needs = |job: &str| -> Vec<String> {
        out["jobs"][job]["needs"]
            .as_vec()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(needs("b"), ["a__1", "a__2"]);
    assert_eq!(needs("c"), ["b", "a__1", "a__2"]);
    assert_eq!(
        out["jobs"]["a__2"]["strategy"]["matrix"]["include"][0]["node"].as_i64(),
        Some(20)
    );
}

#[test]
fn matrix_of_numbers_and_booleans() {
    assert_eq!(
        legs(
            load("node: [18, 20]\nexperimental: [true, false]")
                .as_hash()
                .unwrap()
        )
        .unwrap()
        .len(),
        4
    );
}

#[test]
fn exclude_that_removes_everything_leaves_the_job_to_act() {
    let wf = "\
on: push
jobs:
  t:
    runs-on: ubuntu-latest
    strategy: {matrix: {os: [a], exclude: [{os: a}]}}
    steps: [{run: echo}]
";
    let out = load(&inject(wf, "n").unwrap());
    assert!(out["jobs"]["t"]["steps"].as_vec().is_some());
    assert!(out["jobs"]["t__1"].is_badvalue());
}

#[test]
fn refuses_reusable_workflow_calls() {
    let act = Act::new(config::Act::default());

    for uses in [
        "./.github/workflows/other.yml",
        "acme/ci/.github/workflows/build.yml@v1",
    ] {
        let wf = format!("on: push\njobs:\n  call:\n    uses: {uses}\n");
        let err = act.prepare(&wf, "ci.yml").unwrap_err();
        assert!(err.contains("reusable workflow"), "{err}");
    }
}

#[test]
fn the_artifact_server_listens_on_the_run_network_gateway_or_loopback() {
    assert_eq!(gateway("172.18.0.1 "), Some([172, 18, 0, 1].into()));
    assert_eq!(gateway("fd00::1 10.1.0.1"), Some([10, 1, 0, 1].into()));
    assert_eq!(gateway("8.8.8.8"), None);
    assert_eq!(gateway(""), None);
    assert_eq!(artifact_addr(None), "127.0.0.1");
    let net = Network {
        name: network_name(Path::new("/x")),
        gateway: [172, 18, 0, 1].into(),
    };
    let want = if cfg!(target_os = "macos") {
        "127.0.0.1"
    } else {
        "172.18.0.1"
    };
    assert_eq!(artifact_addr(Some(&net)), want);
    assert!(net.name.starts_with(&format!("{}net-", home_prefix())));
}

#[test]
fn refuses_docker_options_that_reach_the_host() {
    let with = |job: &str| {
        format!("on: push\njobs:\n  t:\n    runs-on: x\n{job}    steps: [{{run: echo}}]\n")
    };
    let internet = Act::new(config::Act::default()).with_network(config::Network::Internet);

    for options in [
        "-v /:/host",
        "--volume=/x:/y",
        "-v/etc:/e",
        "--network host",
        "--privileged",
        "--cap-add SYS_ADMIN",
        "--sysctl net.ipv4.ip_forward=1",
        "--gpus all",
        "--cpus 2 --runtime runc",
    ] {
        let wf = with(&format!(
            "    container: {{image: node, options: '{options}'}}\n"
        ));
        let err = internet.prepare(&wf, "ci.yml").unwrap_err();
        assert!(err.contains("could reach the host"), "{options}: {err}");
    }

    let service = with("    services: {db: {image: postgres, options: '--network host'}}\n");
    assert!(
        internet
            .prepare(&service, "ci.yml")
            .unwrap_err()
            .contains("service options")
    );
    let quiet = with(
        "    services: {db: {image: postgres, options: '--health-cmd \"pg_isready -U postgres\" --health-interval 10s'}}\n",
    );
    assert!(internet.prepare(&quiet, "ci.yml").is_ok());
    let offline = Act::new(config::Act::default());
    assert!(
        offline
            .prepare(&quiet, "ci.yml")
            .unwrap_err()
            .contains("need a network")
    );
    let bind = with("    container: {image: node, volumes: ['/:/host']}\n");
    let err = internet.prepare(&bind, "ci.yml").unwrap_err();
    assert!(err.contains("sets volumes"), "{err}");
    for job in [
        "    container: {image: node, options: '${{ matrix.o }}'}\n",
        "    services: {db: {image: postgres, options: '${{ matrix.o }}'}}\n",
        "    services: ${{ fromJSON(inputs.s) }}\n",
        // The image exemption holds only at a container's own top level
        "    services: {db: {image: p, ports: [{image: '${{ matrix.o }}'}]}}\n",
        "    container: ${{ fromJSON(inputs.c) }}\n",
        "    container: {image: node, options: '--cpus 2 \\\\ --privileged'}\n",
        "    container: {image: node, options: '--cpus \"2'}\n",
        "    container: {image: node, options: ['--privileged']}\n",
    ] {
        assert!(internet.prepare(&with(job), "ci.yml").is_err(), "{job}");
    }

    for job in [
        "    strategy: {matrix: {image: [node, 'python:3']}}\n    container: ${{ matrix.image }}\n",
        "    container: {image: '${{ matrix.image }}', env: {A: '${{ matrix.a }}'}}\n",
    ] {
        assert!(internet.prepare(&with(job), "ci.yml").is_ok(), "{job}");
    }

    // A matrix value can be a whole container mapping with its own options
    let smuggled = "    strategy: {matrix: {c: [{image: node, options: '--privileged'}]}}\n    container: ${{ matrix.c }}\n";
    let err = internet.prepare(&with(smuggled), "ci.yml").unwrap_err();
    assert!(err.contains("--privileged"), "{err}");
    let unresolved = "    container: ${{ matrix.image }}\n";
    assert!(internet.prepare(&with(unresolved), "ci.yml").is_err());

    let service_bind = with("    services: {db: {image: postgres, volumes: ['/etc:/e']}}\n");
    let err = internet.prepare(&service_bind, "ci.yml").unwrap_err();
    assert!(err.contains("sets volumes"), "{err}");
    assert!(
        offline
            .prepare(
                &with("    container: {image: node, options: '--cpus 2'}\n"),
                "ci.yml"
            )
            .is_ok()
    );
}

#[test]
fn allows_only_the_listed_docker_options_in_every_form() {
    let internet = Act::new(config::Act::default()).with_network(config::Network::Internet);
    let check = |options: &str| {
        // Debug quoting is also a valid YAML double-quoted string
        let wf = format!(
            "on: push\njobs:\n  t:\n    runs-on: x\n    container: {{image: node, options: {options:?}}}\n    services: {{db: {{image: pg, options: {options:?}}}}}\n    steps: [{{run: echo}}]\n"
        );
        internet.prepare(&wf, "ci.yml")
    };

    for refused in [
        "--privileged",
        "--privileged=true",
        "--cap-add ALL",
        "--cap-add=SYS_PTRACE",
        "--cap-drop ALL",
        "--pid host",
        "--pid=host",
        "--ipc host",
        "--uts host",
        "--userns host",
        "--cgroupns host",
        "--cgroup-parent x",
        "--network host",
        "--network=container:x",
        "--net host",
        "--add-host h:10.0.0.1",
        "--dns 1.1.1.1",
        "-p 80:80",
        "--publish-all",
        "--device /dev/kvm",
        "--device-cgroup-rule a",
        "--gpus all",
        "--security-opt seccomp=unconfined",
        "--security-opt apparmor=unconfined",
        "--mount type=bind,src=/,dst=/host",
        "-v /:/host",
        "--volume /:/host",
        "--volumes-from x",
        "--tmpfs /x",
        "--env-file /etc/passwd",
        "--label-file /etc/passwd",
        "--runtime runc",
        "--sysctl a=b",
        "--group-add 0",
        "--link x",
        "--log-driver none",
        "--name x",
        "-it",
        "-eA=1",
        "--cpus 2 -v/:/host",
        // Quotes hide no flag, since act's splitter strips them too
        "\"--privileged\"",
        "'--privileged'",
        "--cpus=2 '--priv'ileged",
        "--health-cmd \"--privileged\"",
        // Characters act's splitter reads differently, and expressions act would expand later
        "--cpus 2 \\ --privileged",
        "--cpus 2\n--privileged",
        "--cpus 2\t--privileged",
        "--cpus 2; --privileged",
        "--cpus $(x)",
        "--cpus ${{ matrix.c }}",
        "--cpus `x`",
        "--cpus \"2",
    ] {
        let err = check(refused).expect_err(refused);
        assert!(err.contains("options"), "{refused:?}: {err}");
    }

    for allowed in [
        "--cpus 2",
        "--memory=4g -m 4g --memory-swap 8g --shm-size 2g",
        "--env A=1 -e B=2",
        "--env \"A=a b --privileged\"",
        "--health-cmd \"pg_isready -U postgres\" --health-interval 10s --health-timeout 5s --health-retries 5 --health-start-period 1s",
        "--no-healthcheck",
        "--user 1000:1000 -u 0 --workdir /w -w /w --entrypoint sh",
        "--hostname h -h h --ulimit nofile=1024:2048 --init",
    ] {
        assert!(check(allowed).is_ok(), "{allowed:?}: {:?}", check(allowed));
    }

    // A matrix leg that becomes a whole container is checked as the value it becomes
    let wf = "on: push\njobs:\n  t:\n    runs-on: x\n    strategy: {matrix: {c: [{image: a, options: '--cpus 1'}, {image: b, volumes: ['/:/h']}]}}\n    container: ${{ matrix.c }}\n    steps: [{run: echo}]\n";
    let err = internet.prepare(wf, "ci.yml").unwrap_err();
    assert!(err.contains("sets volumes"), "{err}");
}

#[test]
fn evaluates_simple_continue_on_error_conditions_per_leg() {
    let leg = load("ruby: 3.4-head\nos: ubuntu\nn: 3")
        .as_hash()
        .unwrap()
        .clone();
    let cases = [
        ("${{ endsWith(matrix.ruby, 'head') }}", Some(true)),
        ("${{ startsWith(matrix.ruby, '3.3') }}", Some(false)),
        ("${{ contains(matrix.ruby, '4-h') }}", Some(true)),
        ("${{ matrix.os == 'ubuntu' }}", Some(true)),
        ("${{ matrix.os != 'ubuntu' }}", Some(false)),
        ("${{ matrix.n == '3' }}", Some(true)),
        ("${{ true }}", Some(true)),
        // Anything else stays unknown, so the failure counts
        ("${{ matrix.missing == 'x' }}", None),
        ("${{ github.ref == 'refs/heads/main' }}", None),
        ("${{ fromJSON(inputs.allow) }}", None),
        ("plain", None),
    ];

    for (expr, want) in cases {
        assert_eq!(matrix_condition(expr, &leg), want, "{expr}");
    }

    let wf = "\
on: push
jobs:
  t:
    runs-on: ubuntu-latest
    continue-on-error: ${{ endsWith(matrix.ruby, 'head') }}
    strategy: {matrix: {ruby: ['3.4', head]}}
    steps: [{run: echo}]
";
    let out = inject(wf, "n").unwrap();
    let allowed = workflow::allowed_failures(&out).unwrap();
    assert_eq!(allowed.into_iter().collect::<Vec<_>>(), ["t__2"]);
}

#[test]
fn refuses_engines_with_known_escapes() {
    let ok = "Docker Desktop 4.90.0 (238679)|Engine=29.7.2;containerd=v2.3.3;runc=1.4.3;";
    assert_eq!(engine_problem(ok), None);
    assert!(
        engine_problem("Docker Desktop 4.44.2 (1)|runc=1.4.3;")
            .unwrap()
            .contains("CVE-2025-9074")
    );
    assert_eq!(
        engine_problem("Docker Desktop 4.44.3 (1)|runc=1.3.3;"),
        None
    );

    for runc in ["1.1.12", "1.2.7", "1.3.2", "v1.0.0", "1.4.0-rc.2"] {
        let report = format!("Docker Engine - Community|runc={runc};");
        assert!(engine_problem(&report).is_some(), "{runc}");
    }

    for runc in ["1.2.8", "1.3.3", "1.4.0-rc.3", "1.4.0", "v1.5.1"] {
        let report = format!("Docker Engine - Community|runc={runc};");
        assert_eq!(engine_problem(&report), None, "{runc}");
    }

    // An engine without runc, such as one on crun, is not a known problem
    assert_eq!(engine_problem("Podman Engine|Podman Engine=5.0.0;"), None);
}

#[test]
fn rejects_non_workflows() {
    assert!(inject("- a\n- b\n", "n").is_err());
    assert!(inject("jobs: {}\n", "n").is_err());
    assert!(inject("on: push\n", "n").is_err());
}

#[test]
fn prints_job_outputs_in_a_last_step() {
    let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    outputs:
      version: ${{ steps.v.outputs.version }}
      bad name: x
    steps:
      - id: v
        run: echo version=1 >> $GITHUB_OUTPUT
";
    let out = load(&inject(wf, "n").unwrap());
    let steps = out["jobs"]["a"]["steps"].as_vec().unwrap();
    let printed = &steps[steps.len() - 2];
    assert_eq!(
        printed["env"]["SHIPCHECK_OUTPUT_0"].as_str(),
        Some("${{ steps.v.outputs.version }}")
    );
    assert!(printed["env"]["SHIPCHECK_OUTPUT_1"].is_badvalue());
    assert!(printed["run"].as_str().unwrap().contains("'version'"));
}

#[test]
fn reads_each_steps_summary_from_the_log() {
    // As act 0.2.89 logs two steps that wrote a summary, with step output between them
    let log = "[ci.yml/t] \u{2b50} Run Main printf\n\
                   [ci.yml/t]   \u{2699}  Summary - | a | b |\n\
                   |---|---|\n\
                   | 1 | 2 |\n\
                   [ci.yml/t] unable to get git ref\n\
                   [ci.yml/t]   | [x/y]   \u{2699}  Summary - forged\n\
                   [ci.yml/t]   \u{2699}  Summary - second\n\
                   [ci.yml/t] \u{2b50} Run Complete job\n";
    assert_eq!(read_summary(log), "| a | b |\n|---|---|\n| 1 | 2 |\nsecond");
    assert_eq!(read_summary("[ci.yml/t] nothing\n"), "");
    let long_line = format!("[w/j]   \u{2699}  Summary - {}\n", "\u{e9}".repeat(20_000));
    assert!(read_summary(&long_line).ends_with("[...cut at 20000 bytes...]"));
}

#[test]
fn reads_outputs_from_the_log() {
    // Lines as the outputs step prints them, plus act echoing the command first
    let log = "[CI/a] | printf 'shipcheck-output %s %s\\n' 'v' \"$(...)\"\n\
                   [CI/a]   | shipcheck-output v 6120620a27712720247820c3a9\n\
                   [CI/a]   | shipcheck-output e \n\
                   [CI/a]   | shipcheck-output odd 612\n";
    let out = read_outputs(log);
    assert_eq!(out["v"], "a b\n'q' $x é");
    assert_eq!(out["e"], "");
    assert!(!out.contains_key("odd"));
    assert_eq!(out.len(), 2);
}

fn outputs(entries: &[(&str, &[(&str, &str)])]) -> BTreeMap<String, BTreeMap<String, String>> {
    entries
        .iter()
        .map(|(job, kv)| {
            let kv = kv.iter().map(|(k, v)| (k.to_string(), v.to_string()));
            (job.to_string(), kv.collect())
        })
        .collect()
}

#[test]
fn isolates_one_job_with_its_needs_as_literals() {
    let wf = "\
name: CI
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    steps: [{run: echo test}]
  build:
    runs-on: ubuntu-latest
    steps: [{run: echo build}]
  deploy:
    needs: [test__1, build]
    if: needs.build.result == 'success'
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ needs.build.outputs.tag }} ${{ needs.test.outputs.os }} ${{ needs.build.outputs.none }}
";
    let needs = outputs(&[("test__1", &[("os", "it's")]), ("build", &[("tag", "v1")])]);
    let out = load(&isolate(wf, "ci.yml", "deploy", &needs, "abcd", Some("5eed")).unwrap());
    assert_eq!(out["name"].as_str(), Some("CI"), "github.workflow stays");
    assert_eq!(out["jobs"]["deploy"]["name"].as_str(), Some("deploy abcd"));
    let jobs = out["jobs"].as_hash().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &out["jobs"]["deploy"];
    assert!(job["needs"].is_badvalue());
    assert_eq!(job["if"].as_str(), Some("'success' == 'success'"));
    let guard = job["steps"][0]["run"].as_str().unwrap();
    assert!(guard.contains("/.git/shipcheck-nonce") && guard.contains("!= 5eed ]"));
    assert_eq!(job["steps"][0]["shell"].as_str(), Some("sh"));
    assert_eq!(
        job["steps"][1]["run"].as_str(),
        Some(
            "echo ${{ fromJSON('\"v1\"') }} ${{ fromJSON('\"it\\u0027s\"') }} ${{ fromJSON('\"\"') }}"
        )
    );
}

#[test]
fn outputs_reach_the_next_job_as_literal_strings() {
    let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps: [{run: echo a}]
  b:
    needs: a
    if: needs.a.outputs.v != ''
    runs-on: ubuntu-latest
    env:
      E: ${{ needs.a.outputs.v }}
    steps:
      - run: echo needs.a.outputs.v
      - run: echo \"${{ needs.a.outputs.v }}\"
";
    let value = "x' }} ${{ secrets.S }} ${{ '\n- run: evil\nkey: [a, {b: \"c\n#";
    let needs = outputs(&[("a", &[("v", value)])]);
    let out = load(&isolate(wf, "ci.yml", "b", &needs, "t", Some("n")).unwrap());
    let literal = literal(value);
    // Nothing of the value can end the expression or start one, wherever act scans for them
    let inner = &literal["fromJSON('".len()..literal.len() - "')".len()];
    assert!(!inner.contains(['\'', '{', '}', '$', '\n']), "{literal}");
    assert_eq!(serde_json::from_str::<String>(inner).unwrap(), value);
    let job = &out["jobs"]["b"];
    assert_eq!(out["jobs"].as_hash().unwrap().len(), 1);
    assert_eq!(
        job["steps"].as_vec().unwrap().len(),
        3,
        "the checkout step and the two steps"
    );
    assert_eq!(job["if"].as_str().unwrap(), format!("{literal} != ''"));
    // act evaluates a job's env twice, so the value moves to each step's env
    assert!(job["env"]["E"].is_badvalue());

    for step in &job["steps"].as_vec().unwrap()[1..] {
        assert_eq!(
            step["env"]["E"].as_str().unwrap(),
            format!("${{{{ {literal} }}}}")
        );
    }
    assert_eq!(
        job["steps"][2]["run"].as_str().unwrap(),
        format!("echo \"${{{{ {literal} }}}}\"")
    );
    // Outside an expression GitHub reads it as plain text, and so must shipcheck, or the value
    // would land where act expands `${{ }}`
    assert_eq!(
        job["steps"][1]["run"].as_str(),
        Some("echo needs.a.outputs.v")
    );
}

#[test]
fn refuses_needs_in_an_env_it_cannot_move() {
    let needs = outputs(&[("a", &[("v", "${{ secrets.S }}")])]);
    let with = |job_env: &str, step: &str| {
        format!(
            "on: push\njobs:\n  a: {{runs-on: x, steps: [{{run: echo}}]}}\n  b:\n    needs: a\n    runs-on: x\n{job_env}    steps:\n      - {step}\n"
        )
    };

    for wf in [
        with("    env: ${{ needs.a.outputs.v }}\n", "run: echo"),
        with(
            "    env: {E: '${{ needs.a.outputs.v }}'}\n",
            "{run: echo, env: '${{ fromJSON(x) }}'}",
        ),
        with(
            "    env: {E: '${{ needs.a.outputs.v }}'}\n",
            "{run: echo, env: [x]}",
        ),
    ] {
        let err = isolate(&wf, "ci.yml", "b", &needs, "t", Some("n")).unwrap_err();
        assert!(err.contains("not a mapping"), "{wf}: {err}");
    }

    // An env with nothing from needs is left as it is
    let plain = with("    env: ${{ fromJSON(x) }}\n", "{run: echo, env: [x]}");
    assert!(isolate(&plain, "ci.yml", "b", &needs, "t", Some("n")).is_ok());
    let empty = with(
        "    env: {E: '${{ needs.a.outputs.v }}'}\n",
        "{run: echo, env: }",
    );
    let out = load(&isolate(&empty, "ci.yml", "b", &needs, "t", Some("n")).unwrap());
    assert!(out["jobs"]["b"]["steps"][1]["env"]["E"].as_str().is_some());
}

#[test]
fn refuses_needs_it_cannot_pass() {
    let wf = "\
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps: [{run: echo a}]
  b:
    needs: a
    runs-on: ubuntu-latest
    steps:
      - run: echo '${{ toJSON(needs) }}'
";
    let needs = outputs(&[("a", &[])]);
    let err = isolate(wf, "ci.yml", "b", &needs, "t", Some("n")).unwrap_err();
    assert!(err.contains("toJSON(needs)"), "{err}");
    // Words that only contain `needs`, and `needs` in a script, are fine
    let ok = wf.replace(
        "toJSON(needs)",
        "steps.needs_x.outputs.y }} needs.a.result ${{ inputs.needs",
    );
    assert!(isolate(&ok, "ci.yml", "b", &needs, "t", Some("n")).is_ok());
    assert!(isolate(wf, "ci.yml", "missing", &needs, "t", Some("n")).is_err());
}

#[test]
fn volume_names_are_docker_safe() {
    let name = volume_name("Acme/App.rs", "/root/.rustup");
    assert!(name.starts_with(&home_prefix()) && name.contains("-acme-app-rs-"));
    assert!(name.ends_with("-root--rustup"));
    assert!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    );
}

#[test]
fn the_prune_never_lists_a_current_pin() {
    for (_, image) in DEFAULT_PLATFORMS {
        assert!(!gc::OLD_PINS.contains(&image));
    }

    assert!(!gc::OLD_PINS.contains(&BUSYBOX));
}

#[test]
fn slot_paths_get_a_volume_per_slot_and_job() {
    let act = config::Act::default();
    let run = Path::new("/r");
    let a = mounts(&act, "o/r", "aaaa", run).unwrap();
    let b = mounts(&act, "o/r", "bbbb", run).unwrap();
    let apt = |m: &[String]| {
        m.iter()
            .find(|m| m.ends_with(":/var/lib/apt/lists"))
            .cloned()
    };
    assert_ne!(apt(&a), apt(&b));
    assert!(apt(&a).unwrap().contains("-aaaa-var-lib-apt-lists:"));
    let npm = |m: &[String]| {
        m.iter()
            .find(|m| m.ends_with(":/home/runner/.npm"))
            .cloned()
    };
    assert_eq!(npm(&a), npm(&b));
    assert_eq!(a.last().unwrap(), "--mount 'type=volume,dst=/r/.venv'");
}

#[test]
fn volume_prefixes_never_overlap_between_repos() {
    let (ab, abc) = (volume_prefix("a/b"), volume_prefix("a/b-c"));
    assert!(!volume_name("a/b-c", "/x").starts_with(&ab));
    assert_eq!(
        venv_mount(Path::new("/Users/a/Application Support/r")).unwrap(),
        "--mount 'type=volume,dst=/Users/a/Application Support/r/.venv'"
    );
    assert!(venv_mount(Path::new("/a'b")).is_err());
    assert!(!volume_name("a/b", "/x").starts_with(&abc));
    assert!(volume_name("a/b", "/x").starts_with(&ab));
}

#[test]
fn goes_offline_only_when_every_cached_action_resolves_its_ref() {
    let dir = tempfile::tempdir().unwrap();
    assert!(cached_actions_resolve(&dir.path().join("none")));

    let action = dir.path().join("o-a@v1");
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&action)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    fs::create_dir_all(&action).unwrap();
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "a"]);
    git(&["tag", "v1"]);
    fs::create_dir(dir.path().join("workspace")).unwrap();
    assert!(cached_actions_resolve(dir.path()));

    fs::create_dir(dir.path().join("o-b@v2")).unwrap();
    assert!(
        !cached_actions_resolve(dir.path()),
        "a clone that lost its ref"
    );
}
