use super::*;
use crate::base;
use crate::setup::tests;
use crate::workflow;

fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();

    for (name, text) in files {
        fs::create_dir_all(dir.path().join(name).parent().unwrap()).unwrap();
        fs::write(dir.path().join(name), text).unwrap();
    }

    dir
}

/// Collects what apply reports
#[derive(Default)]
struct Said(Vec<String>);

impl Report for Said {
    fn start(&mut self, _: &str) {}

    fn done(&mut self, mark: Mark, line: &str) {
        let mark = match mark {
            Mark::Ok => "ok",
            Mark::Warn => "!",
        };
        self.0.push(format!("{mark} {line}"));
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature apply takes for its finish step"
)]
fn finish_nothing(say: &mut dyn FnMut(&str)) -> Result<(), String> {
    say("wrote hooks");
    Ok(())
}

const CI: &str =
    "on:\n  pull_request:\n  push:\njobs:\n  test:\n    steps:\n      - run: cargo test\n";
const LINT: &str =
    "on: [push, pull_request]\njobs:\n  lint:\n    steps:\n      - run: cargo clippy\n";

// This repository's own files, from the init its author ran
const SHIPCHECK_TOML: &str = include_str!("../../tests/fixtures/own/shipcheck.toml");
const OWN_CI: &str = include_str!("../../tests/fixtures/own/ci.yml");
const OWN_E2E: &str = include_str!("../../tests/fixtures/own/e2e.yml");
const OWN_LEAKS: &str = include_str!("../../tests/fixtures/own/leaks.yml");

/// `OWN_CI` without the step that sets `AppArmor` up with `sudo`, which keeps the test job on GitHub
fn own_ci() -> String {
    let start = OWN_CI.find("      - timeout-minutes: 5").unwrap();
    let end = OWN_CI.find("      - run: cargo test").unwrap();
    format!("{}{}", &OWN_CI[..start], &OWN_CI[end..])
}

#[test]
fn variants_drop_forced_color_then_turn_color_off() {
    assert_eq!(
        variants("cargo test --color=always && npx jest --color always"),
        [
            "cargo test && npx jest",
            "export NO_COLOR=1 FORCE_COLOR=0 PY_COLORS=0 TERM=dumb && cargo test && npx jest",
        ]
    );
    assert_eq!(
        variants("pytest -c always.ini"),
        ["export NO_COLOR=1 FORCE_COLOR=0 PY_COLORS=0 TERM=dumb && pytest -c always.ini"]
    );
    assert_eq!(variants("ls --color=yes; make -c always")[0], "ls; make");
}

#[test]
fn a_missing_command_is_no_flake_and_names_its_fix() {
    for (log, tool) in [
        (
            "$ x\nbash: line 1: autoreconf: command not found\n",
            "autoreconf",
        ),
        ("zsh: command not found: pnpm", "pnpm"),
        ("sh: 1: pmccabe: not found", "pmccabe"),
    ] {
        assert!(deterministic(log), "{log}");
        assert_eq!(missing_tool(log).as_deref(), Some(tool), "{log}");
        assert!(
            reason(log).ends_with(&format!("fix: install {tool}")),
            "{log}"
        );
    }

    assert!(deterministic("open /dev/ptmx: Operation not permitted"));
    assert!(!deterministic("error: 2 tests failed"));
    assert_eq!(missing_tool("error: 2 tests failed"), None);
    let log = (1..=20).fold(String::new(), |mut s, n| {
        let _ = writeln!(s, "line {n}");
        s
    });
    let why = reason(&log);
    assert!(why.starts_with("failed when init tried it:"), "{why}");
    assert!(why.contains("line 15") && why.contains("line 20") && !why.contains("line 14"));
    assert!(!why.contains("fix:"));
}

#[test]
fn coverage_compares_commands_not_names() {
    // ci.yml's test job, without its service job: fmt, clippy and test run its three commands
    let own = own_ci();
    let ci = own.split("\n  service:").next().unwrap();
    let dir = repo(&[
        (".github/workflows/ci.yml", ci),
        ("shipcheck.toml", SHIPCHECK_TOML),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let ci = &plan.workflows[0];
    assert_eq!(
        ci.yes,
        "on PRs shipcheck runs shipcheck/clippy, shipcheck/fmt, shipcheck/test, GitHub keeps running ci.yml on pushes to main, and on PRs into other branches and on fork PRs"
    );
    // `on:` stays, so a fork's PR, which the daemon never runs, still gets the checks
    assert!(!ci.diff.contains("-  pull_request:"), "{}", ci.diff);
    assert!(ci.diff.contains("&& (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\""), "{}", ci.diff);
}

#[test]
fn a_check_of_part_of_a_job_does_not_cover_it() {
    // curl's checkdocs.yml proselint job: a check of its first step alone passed where proselint never ran
    let ci = "on:\n  pull_request:\n  push:\njobs:\n  proselint:\n    steps:\n      - run: trim.pl\n      - name: check prose\n        run: |\n          cat <<JSON > rc.json\n          {}\n          JSON\n          proselint check --config rc.json docs\n";
    let dir = repo(&[
        (".github/workflows/ci.yml", ci),
        (
            "shipcheck.toml",
            "runner = \"host\"\n[host]\nchecks = { proselint = \"trim.pl\" }\n",
        ),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    // Not gated: GitHub keeps running it on PRs
    assert!(plan.workflows.is_empty());
    assert_eq!(
        plan.notes,
        [
            "ci.yml: GitHub keeps running it on PRs, for proselint, and no job of it runs only in shipcheck"
        ]
    );
}

#[test]
fn a_job_the_config_runs_with_act_is_gated_like_a_host_check() {
    let ci = "on:\n  pull_request:\n  push:\njobs:\n  lint:\n    steps:\n      - run: make lint\n  proselint:\n    steps:\n      - run: |\n          cat <<JSON > rc.json\n          {}\n          JSON\n          proselint check --config rc.json docs\n";
    let dir = repo(&[
        (".github/workflows/ci.yml", ci),
        (
            "shipcheck.toml",
            "runner = \"host\"\n[host]\nchecks = { lint = \"make lint\" }\n\n[jobs.\"ci.yml/proselint\"]\nrunner = \"act\"\n",
        ),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let ci = &plan.workflows[0];
    assert!(
            ci.yes.starts_with("on PRs shipcheck runs shipcheck/lint, shipcheck/proselint, GitHub keeps running ci.yml on pushes to main"),
            "{}",
            ci.yes
        );
    // Both jobs keep the fork clause, so a fork's PR still runs them on GitHub
    assert_eq!(
        ci.diff
            .matches("github.event.pull_request.head.repo.fork")
            .count(),
        2,
        "{}",
        ci.diff
    );
    assert!(plan.github.is_empty(), "{:?}", plan.github);
}

#[test]
fn a_job_whose_check_fails_its_trial_stays_on_github_untried_by_act() {
    let ci = "on: pull_request\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n  broken:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make fails-in-trial\n";
    let dir = repo(&[(".github/workflows/ci.yml", ci)]);
    let machine = setup::Machine {
        docker: true,
        ..setup::Machine::default()
    };
    let (text, said) = setup::propose(dir.path(), &machine).unwrap();
    let mut plan =
        super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    plan.failed.clear();
    let (text, said) = super::tried(
        dir.path(),
        &machine,
        text.unwrap(),
        said,
        &mut Said::default(),
        &mut plan,
    )
    .unwrap()
    .unwrap();
    // act would run it without a trial, so it is written nowhere and GitHub keeps it
    assert!(!text.contains("broken"), "{text}");
    assert!(text.contains("lint = \"make lint\""), "{text}");
    assert_eq!(plan.failed.len(), 1, "{:?}", plan.failed);
    assert!(
        said.contains("ci.yml, job broken: failed when init tried it"),
        "{said}"
    );
}

#[test]
fn coverage_names_the_jobs_no_check_runs() {
    let dir = repo(&[
        (".github/workflows/ci.yml", &own_ci()),
        (".github/workflows/e2e.yml", OWN_E2E),
        (".github/workflows/leaks.yml", OWN_LEAKS),
        ("shipcheck.toml", SHIPCHECK_TOML),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let files: Vec<&str> = plan.workflows.iter().map(|w| w.file.as_str()).collect();
    // Without Docker, e2e.yml's matrix stays on GitHub, which tests see on every machine
    // leaks.yml's job has no command a check could run, so it stays on GitHub too
    assert_eq!(files, ["ci.yml"]);
    assert!(
        plan.notes[0].starts_with("e2e.yml: GitHub keeps running it on PRs"),
        "{:?}",
        plan.notes
    );
    assert_eq!(
        plan.notes[1],
        "leaks.yml: GitHub keeps running it on PRs, for gitleaks, and no job of it runs only in shipcheck"
    );
    // A job no check runs stays on GitHub ungated, so a PR still runs it there
    assert_eq!(
        plan.workflows[0].yes,
        "on PRs shipcheck runs shipcheck/clippy, shipcheck/fmt, shipcheck/test, GitHub keeps running service on PRs and all of ci.yml on pushes to main, and on PRs into other branches and on fork PRs"
    );
    assert!(plan.workflows[0].diff.contains("&& (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\""));
}

#[test]
fn apply_writes_only_what_is_answered() {
    let dir = repo(&[
        (".github/workflows/ci.yml", CI),
        (".github/workflows/lint.yml", LINT),
        (
            "shipcheck.toml",
            "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\nclippy = \"cargo clippy\"\n",
        ),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let mut a = plan.recommended();
    assert_eq!(a.workflows, [true, true]);

    // Changing step 2, lint.yml, and back again, as the review's "Change a step" does
    plan.skip(Step::Workflow(1), &mut a);
    assert!(
        plan.review(&a)[2].contains("PRs and main: GitHub, as now"),
        "{:?}",
        plan.review(&a)
    );
    plan.choose(Step::Workflow(1), &mut a);
    assert!(plan.review(&a)[2].contains("PRs: shipcheck/clippy, main and forks: GitHub"));
    plan.skip(Step::Workflow(1), &mut a);

    let mut said = Said::default();
    let next = apply(dir.path(), &plan, &a, &mut finish_nothing, &mut said).unwrap();
    assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            CI.replace("  test:\n", "  test:\n    if: (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n")
        );
    assert_eq!(
        fs::read_to_string(dir.path().join(".github/workflows/lint.yml")).unwrap(),
        LINT
    );
    assert_eq!(
        said.0,
        ["ok wrote .github/workflows/ci.yml", "ok wrote hooks"]
    );
    assert!(next[0].starts_with("Review shipcheck.toml"), "{next:?}");
}

#[test]
fn a_push_workflow_is_offered_only_when_a_check_runs_its_jobs() {
    const SETUP: &str =
        "on:\n  push:\n    paths: [x]\njobs:\n  setup:\n    steps:\n      - uses: a/b@v1\n";
    const PUSH: &str = "on: push\njobs:\n  test:\n    steps:\n      - run: cargo test\n";
    let dir = repo(&[
        (".github/workflows/ci.yml", PUSH),
        (".github/workflows/setup.yml", SETUP),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let files: Vec<&str> = plan.workflows.iter().map(|w| w.file.as_str()).collect();
    assert_eq!(files, ["ci.yml"]);
    assert_eq!(
        plan.workflows[0].no,
        "GitHub keeps running ci.yml on pushes to every branch"
    );
    assert!(
            plan.notes.contains(&"setup.yml: GitHub keeps running it on pushes to every branch, and no shipcheck check runs its jobs".to_string()),
            "{:?}",
            plan.notes
        );
}

#[test]
fn plan_writes_nothing_and_a_declined_config_sets_nothing_up() {
    let dir = repo(&[(".github/workflows/ci.yml", CI)]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    assert!(!dir.path().join("shipcheck.toml").exists());
    assert_eq!(plan.steps()[0], Step::Config);
    assert_eq!(plan.recommended().workflows, [true]);

    let mut a = plan.recommended();
    a.config = false;
    let mut said = Said::default();
    let mut finished = false;
    apply(
        dir.path(),
        &plan,
        &a,
        &mut |_| {
            finished = true;
            Ok(())
        },
        &mut said,
    )
    .unwrap();
    assert!(!finished);
    assert!(!dir.path().join("shipcheck.toml").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
        CI
    );

    let mut said = Said::default();
    apply(
        dir.path(),
        &plan,
        &plan.recommended(),
        &mut finish_nothing,
        &mut said,
    )
    .unwrap();
    assert_eq!(said.0[0], "ok wrote shipcheck.toml");
    assert!(dir.path().join("shipcheck.toml").exists());
}

#[test]
fn a_job_that_runs_only_on_main_keeps_no_pr_job_ungated() {
    let ci = tests::CI_WITH_RELEASE;
    let config = "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\n";
    let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let mut said = Said::default();
    apply(
        dir.path(),
        &plan,
        &plan.recommended(),
        &mut finish_nothing,
        &mut said,
    )
    .unwrap();
    // release needs test only on main, where the gate still runs it, and e2e stays on PRs
    assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            ci.replace(
                "  test:\n",
                "  test:\n    if: (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n"
            ),
        );
    assert!(
        plan.workflows[0]
            .yes
            .contains("GitHub keeps running e2e on PRs"),
        "{}",
        plan.workflows[0].yes
    );
}

#[test]
fn workflows_keeps_a_matrix_on_github_and_gates_the_rest() {
    let ci = "on:\n  pull_request:\n  push:\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n  tests:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        python: [\"3.12\", \"3.13\"]\n    steps:\n      - run: make test\n";
    let config = "runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n";
    let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let mut said = Said::default();
    apply(
        dir.path(),
        &plan,
        &plan.recommended(),
        &mut finish_nothing,
        &mut said,
    )
    .unwrap();
    assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            ci.replace(
                "  lint:\n",
                "  lint:\n    if: (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n"
            ),
        );
    assert_eq!(
        plan.workflows[0].yes,
        "on PRs shipcheck runs shipcheck/lint, GitHub keeps running tests on PRs and all of ci.yml on pushes to main, and on PRs into other branches and on fork PRs"
    );

    // The matrix needs lint, so lint stays on GitHub too and nothing is gated
    let needs = ci.replace("  tests:\n", "  tests:\n    needs: [lint]\n");
    let dir = repo(&[
        (".github/workflows/ci.yml", &needs),
        ("shipcheck.toml", config),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    assert!(plan.workflows.is_empty());
    assert!(
        plan.notes[0].contains("ci.yml: GitHub keeps running it on PRs, for lint, tests"),
        "{:?}",
        plan.notes
    );
}

#[test]
fn a_job_with_no_command_stays_and_the_rest_is_gated() {
    // rubocop's linting.yml, trimmed: yamllint is an action, so no host check can run it
    let lint = "on:\n  push:\n    branches: [main]\n  pull_request:\njobs:\n  ruby:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n  yaml:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: karancode/yamllint-github-action@v3.0.0\n";
    let config = "runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n";
    let dir = repo(&[
        (".github/workflows/lint.yml", lint),
        ("shipcheck.toml", config),
    ]);
    let plan = super::plan_with(
        dir.path(),
        "main",
        App::Own("1".into()),
        Some(Vec::new()),
        &mut Said::default(),
    )
    .unwrap();
    let change = &plan.workflows[0];
    assert_eq!(
        Some(change.edited.clone()),
        workflow::gate_jobs(lint, &["ruby".to_string()], "main")
    );
    // yaml stays on GitHub for PRs, so the ruleset requires its check from GitHub Actions
    assert_eq!(plan.ruleset.unwrap().github, ["yaml"]);
}

#[test]
fn the_ruleset_requires_every_job_gate_leaves_on_github() {
    // A shipcheck.toml from an init whose trial failed test: test stays on GitHub, and release
    // runs only on main, so no PR waits for it
    let ci = "on: [push, pull_request]\njobs:\n  lint:\n    steps:\n      - run: make lint\n  test:\n    steps:\n      - run: make test\n  release:\n    if: github.ref == 'refs/heads/main'\n    steps:\n      - run: make release\n";
    let config = "runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n";
    let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
    let plan = super::plan_with(
        dir.path(),
        "main",
        App::Own("1".into()),
        Some(Vec::new()),
        &mut Said::default(),
    )
    .unwrap();
    let rule = plan.ruleset.unwrap();
    assert_eq!(rule.names, ["shipcheck/lint"]);
    assert_eq!(rule.github, ["test"]);
}

#[test]
fn the_ruleset_keeps_the_required_checks_of_a_job_whose_names_init_cannot_tell() {
    // hynek/structlog, trimmed: the test matrix's names depend on its include, so init cannot tell them
    let ci = "on: [push, pull_request]\njobs:\n  colors:\n    steps:\n      - run: make colors\n  tests:\n    strategy:\n      matrix:\n        include:\n          - python: '3.12'\n    name: tests on ${{ matrix.python }}\n    steps:\n      - run: tox\n";
    let config = "runner = \"host\"\n[host.checks]\ncolors = \"make colors\"\n";
    let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
    let required = ["tests on 3.12", "tests on 3.13", "shipcheck/colors", "docs"].map(String::from);
    let stays = [("ci.yml".to_string(), "tests".to_string(), None)];
    let rule = ruleset_plan(
        dir.path(),
        config,
        &App::Own("1".into()),
        &stays,
        Some(&required),
    )
    .unwrap();
    assert_eq!(rule.names, ["shipcheck/colors"]);
    // Only the checks of the job whose names init cannot tell, docs is no job of these
    assert_eq!(rule.github, ["tests on 3.12", "tests on 3.13"]);
    assert!(rule.notes.is_empty(), "{:?}", rule.notes);
    // GitHub did not say, so there is no ruleset to write and the branch keeps its rules
    let unread = ruleset_plan(dir.path(), config, &App::Own("1".into()), &stays, None);
    assert!(unread.is_none());
    let plan = super::plan_with(
        dir.path(),
        "main",
        App::Own("1".into()),
        None,
        &mut Said::default(),
    )
    .unwrap();
    assert!(plan.ruleset.is_none());
    let review = plan.review(&plan.recommended());
    assert!(review.iter().any(|l| l == KEPT_RULES), "{review:?}");
}

#[test]
fn a_second_init_leaves_the_workflows_it_gated() {
    let config =
        "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\nclippy = \"cargo clippy\"\n";
    let dir = repo(&[
        (".github/workflows/ci.yml", CI),
        (".github/workflows/lint.yml", LINT),
        ("shipcheck.toml", config),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let mut a = plan.recommended();
    a.workflows = vec![true, false];
    apply(
        dir.path(),
        &plan,
        &a,
        &mut finish_nothing,
        &mut Said::default(),
    )
    .unwrap();
    let gated = fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap();

    // lint.yml, declined the first time, is offered again, ci.yml is not touched
    let again = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let files: Vec<&str> = again.workflows.iter().map(|w| w.file.as_str()).collect();
    assert_eq!(files, ["lint.yml"]);
    assert!(
        again
            .notes
            .contains(&"ci.yml: an earlier init gated test, so init leaves it as it is".into()),
        "{:?}",
        again.notes
    );
    apply(
        dir.path(),
        &again,
        &again.recommended(),
        &mut finish_nothing,
        &mut Said::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
        gated
    );
}

#[test]
fn the_hand_edit_note_gives_the_gate_init_would_write() {
    let flow =
        "on: [push, pull_request]\njobs:\n  test: {runs-on: x, steps: [{run: cargo test}]}\n";
    let config = "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\n";
    let dir = repo(&[
        (".github/workflows/ci.yml", flow),
        ("shipcheck.toml", config),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    let gate = workflow::gate_condition(flow, "main");
    let note = plan.notes.iter().find(|n| n.contains("by hand")).unwrap();
    assert!(note.contains(&format!("`if: {gate}`")), "{note}");
    assert_eq!(workflow::ungate(&gate).as_deref(), Some(""));
}

#[test]
fn a_version_that_does_not_run_is_left_out_with_a_warning() {
    let dir = repo(&[]);
    let text = "runner = \"host\"\n[host]\nversions = [\"rustc +missing --version\", \"cargo --version\"]\n[host.checks]\ntest = \"cargo test\"\n";
    let mut said = Said::default();
    let (tried, versions) = try_checks(dir.path(), text, &mut said).unwrap();
    assert_eq!(versions, ["cargo --version"]);
    assert!(tried["test"].is_ok());
    assert!(
            said.0.contains(
                &"! `rustc +missing --version` does not run, so shipcheck.toml leaves it out of versions".into()
            ),
            "{:?}",
            said.0
        );
}

#[test]
fn init_gates_to_the_branch_runs_and_the_daemon_use() {
    let dir = repo(&[(".github/workflows/ci.yml", CI)]);
    let origin = tempfile::tempdir().unwrap();
    let git = |at: &Path, args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(at)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(origin.path(), &["init", "-q", "--bare", "-b", "master"]);
    git(dir.path(), &["init", "-q", "-b", "master"]);
    git(
        dir.path(),
        &[
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
        ],
    );
    git(
        dir.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    git(
        dir.path(),
        &["push", "-q", "origin", "master", "master:evil"],
    );
    git(dir.path(), &["fetch", "-q", "origin"]);

    // A local origin/HEAD at another branch does not pick the base, the remote does
    git(
        dir.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/evil",
        ],
    );
    assert_eq!(
        base::remote_base(dir.path()).as_deref(),
        Some("origin/master")
    );
    let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
    assert_eq!(plan.base, "master");
    assert!(plan.workflows[0].edited.contains("refs/heads/master"));

    // A remote that does not answer gives no base, never the local origin/HEAD, and init refuses
    git(dir.path(), &["remote", "set-url", "origin", "/nonexistent"]);
    assert_eq!(base::remote_base(dir.path()), None);
    assert!(
        base_branch(dir.path())
            .unwrap_err()
            .contains("again when online")
    );
}

#[test]
fn a_workflow_of_jobs_only_github_can_run_stays_as_it_is() {
    // orch's orch-review.yml, trimmed: on main its `if:` skips the job, so cutting it to main runs it nowhere
    let review = "on:\n  push:\njobs:\n  review-gate:\n    runs-on: ubuntu-latest\n    if: github.ref != format('refs/heads/{0}', github.event.repository.default_branch)\n    steps:\n      - run: ./scripts/lint-frontmatter.sh\n";
    let config = "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\n";
    let dir = repo(&[
        (".github/workflows/ci.yml", CI),
        (".github/workflows/orch-review.yml", review),
        ("shipcheck.toml", config),
    ]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    assert_eq!(plan.workflows.len(), 1);
    assert!(
        plan.notes
            .iter()
            .any(|n| n
                .starts_with("orch-review.yml: GitHub keeps running it on pushes to every branch")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn init_stops_without_anything_to_run_here() {
    // git-cow's ci.yml, trimmed: its only test job is a matrix with a macOS entry
    let ci = "on: [push, pull_request]\njobs:\n  test:\n    strategy:\n      matrix:\n        include:\n          - {os: macos-latest}\n          - {os: ubuntu-latest}\n    runs-on: ${{ matrix.os }}\n    steps:\n      - run: cargo test\n";
    let dir = repo(&[(".github/workflows/ci.yml", ci)]);
    let plan = super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
    assert!(plan.stop.as_deref().unwrap().contains(setup::NOTHING_LOCAL));
    assert!(!dir.path().join("shipcheck.toml").exists());
}

#[test]
fn job_checks_reads_the_workflows() {
    let dir = repo(&[
        (".github/workflows/ci.yml", CI),
        (".github/workflows/lint.yml", LINT),
    ]);
    assert_eq!(
        job_checks(dir.path(), "workflows = [\"ci.yml\", \"lint.yml\"]\n").unwrap(),
        ["shipcheck/test", "shipcheck/lint"]
    );
}

#[test]
fn ruleset_requires_the_apps_check_with_branches_up_to_date() {
    let r = ruleset(
        "5400635",
        &["shipcheck/lint".into(), "shipcheck/test".into()],
        &["MINGW64".into()],
    );
    let rule = &r["rules"][0]["parameters"];
    assert_eq!(rule["strict_required_status_checks_policy"], true);
    assert_eq!(
        rule["required_status_checks"],
        serde_json::json!([
            { "context": "shipcheck/lint", "integration_id": 5_400_635 },
            { "context": "shipcheck/test", "integration_id": 5_400_635 },
            { "context": "MINGW64", "integration_id": 15368 }
        ])
    );
    assert_eq!(r["conditions"]["ref_name"]["include"][0], "~DEFAULT_BRANCH");
    assert_eq!(r["rules"][1]["type"], "pull_request");
}
