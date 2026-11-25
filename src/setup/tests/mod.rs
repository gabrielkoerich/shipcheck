use super::hooks::{Entry, Hooks, MARK, gap_line, stale_hooks};
use super::shell::{checks_nothing, missing_tools, tool_versions, unquoted};
use std::collections::BTreeSet;

use super::*;
use crate::config::{Network, RepoConfig, RunnerKind};
use crate::run;
use crate::runner::host;

mod placement;

#[test]
fn generated_config_parses() {
    let text = config_for(&["ci.yml".into(), "deploy.yml".into()]);
    assert_eq!(
        RepoConfig::parse(&text).unwrap().workflows,
        ["ci.yml", "deploy.yml"]
    );
}

#[test]
fn init_turns_sharding_on() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    fs::write(dir.path().join(".github/workflows/ci.yml"), "on: push").unwrap();
    init(dir.path()).unwrap();
    let text = fs::read_to_string(dir.path().join("shipcheck.toml")).unwrap();
    assert!(RepoConfig::parse(&text).unwrap().shard_tests.enabled());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one case per kind of job, read as one table"
)]
fn init_turns_workflow_jobs_into_host_checks() {
    let ci = r#"on: [push, pull_request]
defaults:
  run:
    shell: bash
jobs:
  test:
    steps:
      - uses: actions/checkout@v4
      - run: pip install -r r.txt
      - run: cargo fmt --check
      - run: cargo build --release
      - run: make ${{ x }}
      - run: cat "$GITHUB_OUTPUT"
      - run: cargo test | tee out || true
      - run: make lint
        continue-on-error: true
      - run: make coverage
        if: github.event_name == 'push'
  service:
    defaults:
      run:
        working-directory: service
    steps:
      - run: bun install
      - run: bun run test
      - run: bunx tsc --noEmit
  mixed:
    steps:
      - run: make
      - run: make check
        working-directory: sub
  clippy:
    steps:
      - run: cargo clippy --all-targets -- -D warnings
  again:
    steps:
      - run: cargo clippy --all-targets -- -D warnings
  deploy-docs:
    steps:
      - run: make docs
  predeploy:
    steps:
      - run: |
          # Seed the keypairs
          mkdir -p target/deploy

          cp key.json target/deploy/key.json
          a || b
      - run: anchor build
  ship:
    steps:
      - run: npm publish --dry-run
  gated:
    if: github.repository == 'a/b'
    steps:
      - run: make
  "a b":
    steps:
      - run: "true"
  setup-only:
    steps:
      - run: pip install -r r.txt
"#;
    let tags = "on:\n  push:\n    tags: [v*]\njobs:\n  release:\n    steps:\n      - run: make\n";
    let (cfg, text, said) = init_with(&[
        (".github/workflows/ci.yml", ci),
        (".github/workflows/tag.yml", tags),
        (
            ".github/workflows/title.yml",
            "on:\n  pull_request_target:\njobs:\n  title:\n    steps:\n      - run: make\n",
        ),
        ("Cargo.toml", ""),
        ("package.json", r#"{"scripts": {"lint": "eslint ."}}"#),
    ]);
    assert_eq!(cfg.runner, RunnerKind::Host);
    assert_eq!(
        checks(&cfg),
        [
            ("clippy", "cargo clippy --all-targets -- -D warnings"),
            ("mixed", "make && (cd sub && make check)"),
            (
                "predeploy",
                "(mkdir -p target/deploy && cp key.json target/deploy/key.json && { a || b; }) && anchor build"
            ),
            (
                "service",
                "cd service && bun install && bun run test && bunx tsc --noEmit"
            ),
        ],
        "{text}"
    );
    // Written in the order found
    assert!(
        text.contains("[host.checks]\nservice = \"cd service"),
        "{text}"
    );
    // Project files give versions, and no check while a workflow gives some
    assert_eq!(
        cfg.host.versions,
        [
            "node --version",
            "npm --version",
            "cargo --version",
            "rustc --version"
        ]
    );
    for line in [
        // A check without its other steps would pass where the job fails
        "  ci.yml, job test: a check would leave out its step `make ${{ x }}`, a step with an expression",
        "  ci.yml, job again: runs the same as clippy",
        "  ci.yml, job deploy-docs: it looks like a deploy, publish or release",
        "  ci.yml, job ship: a step deploys, publishes or releases",
        "  ci.yml, job gated: a job with `if:` or `continue-on-error`, which GitHub may skip",
        "  ci.yml, job a b: the job id is not letters, digits, `_` and `-`",
        "  ci.yml, job setup-only: no run step to check with",
        "  tag.yml: GitHub does not run it for feature branches",
        "  title.yml: it runs on pull_request_target, from the base branch with its secrets, so it stays on GitHub",
        "  Cargo.toml: already in a workflow check: `cargo clippy --all-targets -- -D warnings`",
        "  Cargo.toml: ci.yml, job test runs it on GitHub: `cargo test`",
        "  package.json: no workflow runs it: `npm run lint`",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }
}

#[test]
fn a_script_runs_in_its_package_managers_forms_and_with_flags() {
    for (shell, command) in [
        ("pnpm run lint", "pnpm lint"),
        ("pnpm format && pnpm run lint", "pnpm lint"),
        ("yarn run lint", "yarn lint"),
        ("npm run test", "npm test"),
        ("npm test", "npm test"),
        ("go test -race -tags=integration ./...", "go test ./..."),
        ("(cargo test --workspace)", "cargo test"),
    ] {
        assert!(runs_command(shell, command), "{shell} runs {command}");
    }

    for (shell, command) in [
        ("pnpm run test-unit", "pnpm test"),
        ("npm run test:ci", "npm test"),
        ("bun test", "bun run test"),
        ("go vet ./...", "go test ./..."),
        ("go -C sub test ./...", "go test ./..."),
        ("go test ./cmd/...", "go test ./..."),
    ] {
        assert!(
            !runs_command(shell, command),
            "{shell} does not run {command}"
        );
    }
}

#[test]
fn multi_line_steps_take_only_simple_commands() {
    let ci = r"on: push
jobs:
  ok:
    steps:
      - run: |
          cd tests
          FUZZ=1 cargo run --release
  cont:
    steps:
      - run: |
          cargo test \
            --locked
  heredoc:
    steps:
      - run: |
          cat <<EOF > a
          x
          EOF
  flow:
    steps:
      - run: |
          if [ -f a ]
          then make
          fi
  func:
    steps:
      - run: |
          f() { make; }
          f
";
    let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", ci)]);
    assert_eq!(
        checks(&cfg),
        [("ok", "cd tests && FUZZ=1 cargo run --release")]
    );
    for line in [
        "job cont: a check would leave out its step `cargo test \\`, a step with a `\\` continuation",
        "job heredoc: a check would leave out its step `cat <<EOF > a`, a step with a heredoc",
        "job flow: a check would leave out its step `if [ -f a ]`, a step with shell control flow or a function",
        "job func: a check would leave out its step `f() { make; }`, a step with shell control flow or a function",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }
}

#[test]
fn a_job_runs_the_jobs_it_needs_first() {
    let ci = r"on: push
jobs:
  e2e:
    needs: [integration, build]
    steps:
      - uses: actions/download-artifact@v4
      - run: make e2e
  integration:
    needs: build
    steps:
      - run: make integration
  build:
    needs: setup
    steps:
      - run: make build
  setup:
    steps:
      - run: pip install -r r.txt
";
    let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", ci)]);
    assert_eq!(
        cfg.host.checks["e2e"],
        "(make build) && (make integration) && make e2e"
    );
    assert_eq!(
        cfg.host.checks["integration"],
        "(make build) && make integration"
    );
    assert_eq!(cfg.host.checks["build"], "make build");
    assert!(
        said.contains("(from ci.yml, job e2e, after job build, integration)"),
        "{said}"
    );
    assert!(
        said.contains("job build: needs job setup, which has no check"),
        "{said}"
    );
}

#[test]
fn project_files_give_checks_only_without_workflow_checks() {
    let (cfg, _, _) = init_with(&[
        (
            ".github/workflows/ci.yml",
            "on: push\njobs:\n  a:\n    steps:\n      - run: pip install -r r.txt\n",
        ),
        ("Cargo.toml", ""),
    ]);
    assert_eq!(cfg.host.checks["test"], "cargo test");
}

#[test]
fn workflow_checks_cannot_inject_toml() {
    let ci = r#"on: push
jobs:
  control:
    steps:
      - run: "x\r\u007f"
  test:
    steps:
      - run: "x\n[host]\nchecks = { t = \"true\" }"
  lint:
    steps:
      - run: 'cargo clippy "a" ''b'''
"#;
    let (cfg, text, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
    // The lines of a multi-line step are commands inside one TOML string, never TOML lines
    assert_eq!(
        checks(&cfg),
        [
            ("lint", "cargo clippy \"a\" 'b'"),
            ("test", "x && [host] && checks = { t = \"true\" }")
        ],
        "{text}"
    );
}

#[test]
fn workflow_env_goes_into_the_check() {
    // axum's CI.yml, trimmed
    let ci = r#"on: [push, pull_request]
env:
  CARGO_TERM_COLOR: always
jobs:
  cargo-hack:
    runs-on: ubuntu-24.04
    env:
      # Fail the build if there are any warnings
      RUSTFLAGS: "-D warnings"
    steps:
      - uses: taiki-e/install-action@cargo-hack
      - name: cargo hack check
        run: cargo hack check --each-feature --no-dev-deps --all
  armv5te-unknown-linux-musleabi:
    runs-on: ubuntu-24.04
    steps:
      - name: Check
        env:
          CC: clang
        run: cargo check --all-targets --all-features --target armv5te-unknown-linux-musleabi
"#;
    // rails' check-markdown-api.yml and gin's gin.yml, trimmed
    let rails = r"on: pull_request
jobs:
  markdown_api:
    steps:
      - run: ruby .github/workflows/scripts/check-markdown-api.rb $BASE_SHA $HEAD_SHA
        env:
          BASE_SHA: ${{ github.event.pull_request.base.sha }}
          HEAD_SHA: ${{ github.event.pull_request.head.sha }}
  test:
    env:
      GO111MODULE: on
      TESTTAGS: ${{ matrix.test-tags }}
    steps:
      - run: make test
";
    let (cfg, _, said) = init_with(&[
        (".github/workflows/ci.yml", ci),
        (".github/workflows/rails.yml", rails),
    ]);
    assert_eq!(
        checks(&cfg),
        [
            (
                "armv5te-unknown-linux-musleabi",
                "export CARGO_TERM_COLOR=always && export CC=clang && cargo check --all-targets --all-features --target armv5te-unknown-linux-musleabi"
            ),
            (
                "cargo-hack",
                "export CARGO_TERM_COLOR=always RUSTFLAGS='-D warnings' && cargo hack check --each-feature --no-dev-deps --all"
            ),
            ("test", "export GO111MODULE=on && make test"),
        ]
    );
    for line in [
        "rails.yml, job markdown_api: a check would leave out its step `ruby .github/workflows/scripts/check-markdown-api.rb $BASE_SHA $HEAD_SHA`, a step with an expression",
        "rails.yml, job test: left out variable TESTTAGS, an expression",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }
}

#[test]
fn a_job_that_checks_nothing_is_left_out() {
    // fastapi, axum, ruff, cobra and express workflows, trimmed
    let ci = r#"on: pull_request
jobs:
  pre-commit:
    steps:
      - run: uv sync --locked --extra all
      - run: uv run prek run --from-ref origin/${GITHUB_BASE_REF} --to-ref HEAD
        continue-on-error: true
  test-msrv:
    steps:
      - run: cargo +nightly update -Z minimal-versions
      - run: cargo +${{ env.MSRV }} test --locked --all-features
  docker-build:
    steps:
      - uses: docker/build-push-action@v6
      - run: |
          mkdir -p /tmp/digests
          touch "/tmp/digests/x"
  test-win:
    runs-on: windows-latest
    steps:
      - run: git config --global core.autocrlf input
      - run: go test ./...
  test:
    steps:
      - run: npm config set loglevel error
      - run: python -m pip install pre-commit==4.6.0
      - run: npm install
      - run: npm run test-ci
  rail_inspector:
    steps:
      - run: rm -f Gemfile.lock
      - run: cd tools/rail_inspector && bundle exec rake
"#;
    let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", ci)]);
    assert_eq!(
        checks(&cfg),
        [
            (
                "rail_inspector",
                "rm -f Gemfile.lock && (cd tools/rail_inspector && bundle exec rake)"
            ),
            ("test", "npm install && npm run test-ci"),
        ]
    );
    for line in [
        // A job made of steps a check leaves out stays on GitHub, so init never offers to cut its trigger
        "ci.yml, job pre-commit: it reads HEAD, which on a PR is GitHub's merge commit and HEAD^ its base, and here the branch's own commit",
        "job test-msrv: a check would leave out its step `cargo +${{ env.MSRV }} test --locked --all-features`, a step with an expression",
        "job docker-build: runs Docker, which shipcheck never gives a job",
        "job test-win: runs on Windows, which shipcheck does not support yet",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }
}

#[test]
fn a_step_cd_stays_in_its_step() {
    // fastapi's test-redistribute.yml, trimmed: the install after the `cd` is left out
    let ci = r"on: pull_request
jobs:
  test-redistribute:
    steps:
      - run: python -m build --sdist
      - run: |
          cd dist/fastapi*/
          pip install --group tests --editable .[all]
      - run: |
          cd dist/fastapi*/
          bash scripts/test.sh
      - run: cd dist
      - run: pip wheel --no-deps fastapi*.tar.gz
";
    let (cfg, _, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
    assert_eq!(
        checks(&cfg),
        [(
            "test-redistribute",
            "python -m build --sdist && (cd dist/fastapi*/ && bash scripts/test.sh) && pip wheel --no-deps fastapi*.tar.gz"
        )]
    );
}

#[test]
fn a_job_on_artifacts_no_check_builds_is_left_out() {
    // fastapi's test.yml, trimmed
    let ci = r#"on: pull_request
jobs:
  test:
    strategy:
      matrix:
        python-version: ["3.10", "3.14"]
    steps:
      - run: uv run --no-sync bash scripts/test-cov.sh ${{ matrix.python-version }}
  coverage-combine:
    needs: [test]
    steps:
      - uses: actions/download-artifact@v8
      - run: uv sync --locked --no-dev --group tests --extra all
      - run: uv run coverage combine coverage
      - run: uv run coverage report --fail-under=100
"#;
    let found = detect_with(
        &[(".github/workflows/ci.yml", ci), ("Cargo.toml", "")],
        Docker::Stopped,
    );
    assert!(found.act.is_none());
    let said = found.explain();
    assert!(
        said.contains(
            "job coverage-combine: downloads artifacts from jobs it needs that have no check: test"
        ),
        "{said}"
    );
    // The matrix stays on GitHub, so Cargo.toml's guesses are no checks either
    assert!(found.checks.is_empty(), "{said}");
}

fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();

    for (name, text) in files {
        fs::create_dir_all(dir.path().join(name).parent().unwrap()).unwrap();
        fs::write(dir.path().join(name), text).unwrap();
    }

    dir
}

// axum's CI.yml, trimmed
const AXUM: &str = r#"on:
  push:
    branches: [main]
  pull_request: {}
jobs:
  check:
    runs-on: ubuntu-24.04
    strategy:
      matrix:
        workspace: [".", examples]
    steps:
      - uses: dtolnay/rust-toolchain@beta
      - run: cargo clippy --manifest-path ${{ matrix.workspace }}/Cargo.toml
      - run: cargo fmt --all --check --manifest-path ${{ matrix.workspace }}/Cargo.toml
  check-docs:
    runs-on: ubuntu-24.04
    steps:
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo doc --package axum --all-features --no-deps
  external-types:
    runs-on: ubuntu-24.04
    steps:
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: nightly-2026-03-20
      - run: cargo hack --no-private check-external-types --all-features
  test-nightly:
    runs-on: ubuntu-24.04
    steps:
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: ${{ steps.rust-toolchain.outputs.version }}
      - run: cargo test
        working-directory: axum-macros
  armv5te-unknown-linux-musleabi:
    runs-on: ubuntu-24.04
    steps:
      - uses: dtolnay/rust-toolchain@stable
        with:
          target: armv5te-unknown-linux-musleabi
      - run: cargo check --all-targets --target armv5te-unknown-linux-musleabi
"#;

/// The jobs no host check runs, on GitHub or on act, in that order
fn not_host(found: &Detected) -> Vec<&Kept> {
    found
        .github
        .iter()
        .chain(found.acts.iter().map(|(k, _)| k))
        .collect()
}

/// Whether Docker runs on the test's machine
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Docker {
    Running,
    Stopped,
}

fn detect_with(files: &[(&str, &str)], docker: Docker) -> Detected {
    detect_on(
        repo(files).path(),
        &Machine {
            docker: docker == Docker::Running,
            ..Machine::default()
        },
    )
}

#[test]
fn a_literal_setup_python_version_pins_uv_and_goes_into_the_key() {
    let ci = r"on: pull_request
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-python@v6
        with:
          python-version: '3.13'
      - run: uv run pytest
  typing:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-python@v6
        with:
          python-version-file: pyproject.toml
      - run: uv run mypy
";
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Stopped);
    let command = |name: &str| {
        let check = found.checks.iter().find(|c| c.name == name).unwrap();
        check.command.clone()
    };
    assert_eq!(command("test"), "export UV_PYTHON=3.13 && uv run pytest");
    assert_eq!(command("typing"), "uv run mypy");
    assert!(
        found
            .versions
            .contains(&"uv python find 3.13 --show-version || echo missing".to_string()),
        "{:?}",
        found.versions
    );
    assert!(
        found
            .versions
            .contains(&"uv python find --show-version || echo missing".to_string()),
        "{:?}",
        found.versions
    );

    let job = |version: &str, run: &str| {
        let ci = format!(
            "on: push\njobs:\n  t:\n    steps:\n      - uses: actions/setup-python@v6\n        with:\n          python-version: {version}\n      - run: {run}\n"
        );
        let jobs = workflow::job_runs(&ci);
        setup_python(&jobs[0])
    };
    assert_eq!(job("'3.14t'", "uv run pytest").as_deref(), Some("3.14t"));
    assert_eq!(
        job("3.12.4", "cd x && uvx ruff check").as_deref(),
        Some("3.12.4")
    );
    assert_eq!(job("'3.x'", "uv run pytest"), None);
    assert_eq!(job("pypy-3.11", "uv run pytest"), None);
    assert_eq!(job("${{ matrix.python }}", "uv run pytest"), None);
    assert_eq!(job("'3.13'", "pytest"), None);
}

#[test]
fn a_jdk_this_machine_has_is_pinned_and_a_missing_one_keeps_the_job_on_github() {
    let ci = r"on: pull_request
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-java@v5
        with:
          distribution: temurin
          java-version: |
            11
            21
      - run: ./gradlew test
  native:
    runs-on: ubuntu-latest
    steps:
      - uses: graalvm/setup-graalvm@v1
        with:
          java-version: 25
      - run: ./gradlew nativeTest
";
    let jdk = |version: &str, home: &str, native| Jdk {
        version: version.into(),
        home: home.into(),
        native,
    };
    let found = detect_on(
        repo(&[(".github/workflows/ci.yml", ci)]).path(),
        &Machine {
            jdks: vec![
                jdk("11.0.2", "/jvm/11", false),
                jdk("21.0.5", "/jvm/21", false),
            ],
            ..Machine::default()
        },
    );
    assert_eq!(found.checks.len(), 1);
    assert_eq!(
        found.checks[0].command,
        "export JAVA_HOME=/jvm/21 PATH=/jvm/21/bin:\"$PATH\" && ./gradlew test"
    );
    assert_eq!(found.versions, ["/jvm/21/bin/java -version 2>&1"]);
    assert_eq!(found.github.len(), 1);
    assert_eq!(found.github[0].job, "native");
    assert_eq!(
        found.github[0].why,
        "needs GraalVM 25 with native-image, which this machine does not have"
    );

    // Without a JDK both stay on GitHub
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Stopped);
    assert!(found.checks.is_empty());
    assert_eq!(
        found.github[0].why,
        "needs JDK 11, which this machine does not have"
    );
}

#[test]
fn an_echo_into_a_file_of_the_tree_stays_in_the_check() {
    let ci = r"on: pull_request
jobs:
  latest:
    runs-on: ubuntu-latest
    steps:
      - run: echo 'kotlin.jvm.target.validation.mode=ignore' >> ./gradle.properties
      - run: echo building 2>&1 > /dev/null
      - run: make test
";
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Stopped);
    assert_eq!(
        found.checks[0].command,
        "echo 'kotlin.jvm.target.validation.mode=ignore' >> ./gradle.properties && make test"
    );
}

#[test]
fn a_label_gated_job_that_also_runs_on_main_is_not_said_to_never_run_there() {
    let ci = r"on: pull_request
jobs:
  providers:
    runs-on: ubuntu-latest
    if: github.ref == 'refs/heads/main' || contains(github.event.pull_request.labels.*.name, 'providers')
    steps:
      - run: make test
";
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Stopped);
    assert_eq!(found.github[0].why, PR_ONLY);
    assert!(!PR_ONLY.contains("never runs"), "{PR_ONLY}");
}

/// The config `init` writes for a root holding these files, parsed and validated
#[test]
fn a_docker_job_stays_on_github_and_a_base_only_chain_holds_nothing_back() {
    let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", CI_WITH_RELEASE)]);
    assert_eq!(checks(&cfg), [("test", "cargo test")]);
    for line in [
        "job e2e: runs Docker, which shipcheck never gives a job",
        "job check-release: runs only on the base branch",
        "job build: runs only on the base branch",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }

    // Without the comment the SHA names no toolchain, so the check runs on this machine's, said so
    let bare = CI_WITH_RELEASE.replace(" # stable", "");
    let d = detect_with(&[(".github/workflows/ci.yml", &bare)], Docker::Running);
    assert!(d.checks.iter().any(|c| c.name == "test"));
    assert!(
        d.explain().contains("job test: needs toolchain"),
        "{}",
        d.explain()
    );
}

pub(crate) const CI_WITH_RELEASE: &str = r"on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: dtolnay/rust-toolchain@b725eda525362e7cf34af9a7243cac1e3f01a7ec # stable
      - run: cargo test
  e2e:
    runs-on: ubuntu-latest
    steps:
      - run: docker version
      - run: cargo test --test e2e -- --ignored
  check-release:
    needs: [test, e2e]
    if: github.ref == 'refs/heads/main'
    runs-on: ubuntu-latest
    steps:
      - run: ./next-version.sh
  build:
    needs: check-release
    strategy:
      matrix:
        os: [macos-latest, ubuntu-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - run: cargo build --release
";

fn init_with(files: &[(&str, &str)]) -> (RepoConfig, String, String) {
    let dir = tempfile::tempdir().unwrap();

    for (name, text) in files {
        fs::create_dir_all(dir.path().join(name).parent().unwrap()).unwrap();
        fs::write(dir.path().join(name), text).unwrap();
    }

    let said = init(dir.path()).unwrap().unwrap();
    let text = fs::read_to_string(dir.path().join("shipcheck.toml")).unwrap();
    let cfg = RepoConfig::parse(&text).unwrap();
    let workflows: BTreeSet<String> = cfg.workflows.iter().cloned().collect();
    assert_eq!(cfg.validate(&workflows), Ok(()), "{text}");
    (cfg, text, said)
}

fn checks(cfg: &RepoConfig) -> Vec<(&str, &str)> {
    cfg.host
        .checks
        .iter()
        .map(|(k, v)| (k.as_str(), &**v))
        .collect()
}

#[test]
fn versions_name_each_tool_a_check_starts() {
    let found = |command: &str| Found {
        name: "c".into(),
        command: command.into(),
        from: String::new(),
    };
    let checks = [
        found("git rev-parse HEAD >/dev/null && gitleaks git --redact ."),
        found("{ git ls-files -z '*.sh' | xargs -0 shellcheck -S warning; } && make lint"),
        found("time cargo check && (FUZZ=1 uv run pytest || ./run.sh)"),
    ];
    let known = vec!["cargo --version".to_string()];
    assert_eq!(
        tool_versions(&checks, &known, |t| t != "make"),
        ["gitleaks --version", "shellcheck --version", "uv --version"]
    );
}

#[test]
fn init_detects_package_json_scripts_with_the_lockfiles_manager() {
    let pkg = r#"{"scripts": {"test": "jest", "lint": "eslint ."}}"#;
    for (lock, test, lint, version) in [
        (
            "package-lock.json",
            "npm test",
            "npm run lint",
            "npm --version",
        ),
        ("yarn.lock", "yarn test", "yarn lint", "yarn --version"),
        ("pnpm-lock.yaml", "pnpm test", "pnpm lint", "pnpm --version"),
        ("bun.lock", "bun run test", "bun run lint", "bun --version"),
    ] {
        let (cfg, text, said) = init_with(&[("package.json", pkg), (lock, "")]);
        assert_eq!(cfg.runner, RunnerKind::Host);
        assert!(text.contains("runner = \"host\""));
        assert_eq!(checks(&cfg), [("lint", lint), ("test", test)], "{lock}");
        assert!(cfg.host.versions.contains(&version.to_string()));
        assert_eq!(
            cfg.host.versions.contains(&"node --version".to_string()),
            !lock.starts_with("bun")
        );
        assert!(
            said.contains(&format!("  test = {test}  (from package.json)")),
            "{said}"
        );
    }

    // npm init's placeholder test is no check
    let placeholder = r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#;
    let (cfg, _, _) = init_with(&[("package.json", placeholder), ("Cargo.toml", "")]);
    assert_eq!(checks(&cfg).len(), 2);
}

#[test]
fn init_detects_cargo_python_and_go() {
    let (cfg, _, _) = init_with(&[("Cargo.toml", "[package]\nname = \"a\"\n")]);
    assert_eq!(
        checks(&cfg),
        [
            ("clippy", "cargo clippy --all-targets -- -D warnings"),
            ("test", "cargo test")
        ]
    );
    assert_eq!(cfg.host.versions, ["cargo --version", "rustc --version"]);

    let (cfg, _, _) = init_with(&[("pyproject.toml", ""), ("uv.lock", "")]);
    assert_eq!(checks(&cfg), [("test", "uv run pytest")]);
    assert_eq!(
        cfg.host.versions,
        [
            "uv --version",
            "uv python find --show-version || echo missing"
        ]
    );
    let (cfg, _, _) = init_with(&[("pyproject.toml", "")]);
    assert_eq!(checks(&cfg), [("test", "pytest")]);

    let (cfg, _, _) = init_with(&[("go.mod", "module a\n")]);
    assert_eq!(
        checks(&cfg),
        [("test", "go test ./..."), ("vet", "go vet ./...")]
    );
    assert_eq!(cfg.host.versions, ["go version"]);

    // Two ecosystems, so the second test is named after its tool
    let (cfg, _, _) = init_with(&[("Cargo.toml", ""), ("go.mod", "")]);
    assert_eq!(cfg.host.checks["test"], "cargo test");
    assert_eq!(cfg.host.checks["test-go"], "go test ./...");
}

#[test]
fn init_falls_back_to_act_and_says_why() {
    let (cfg, text, said) = init_with(&[
        (".github/workflows/ci.yml", "on: push"),
        ("package.json", r#"{"scripts": {"build": "tsc"}}"#),
    ]);
    assert_eq!(cfg.runner, RunnerKind::Act);
    assert!(text.starts_with("runner = \"act\"\n"));
    assert_eq!(cfg.workflows, ["ci.yml"]);
    assert!(said.contains("runner = \"act\": no check found"), "{said}");

    let dir = tempfile::tempdir().unwrap();
    assert!(
        init(dir.path())
            .unwrap_err()
            .contains("no .github/workflows")
    );
}

#[test]
fn a_step_that_needs_root_keeps_its_job_on_github() {
    // orch's release.yml test job, trimmed, and an install beside the check in one step
    let ci = r#"on: push
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - name: Create XFS reflink volume
        run: |
          command -v mkfs.xfs >/dev/null || sudo apt-get install -y xfsprogs
          truncate -s 8G /tmp/xfs.img
          mkfs.xfs -m reflink=1 /tmp/xfs.img
          sudo mkdir /mnt/xfs
          sudo mount -o loop /tmp/xfs.img /mnt/xfs
      - name: Run tests with coverage
        run: cargo llvm-cov nextest --lcov --output-path lcov.info
  setup:
    steps:
      - run: |
          sudo apt-get update
          sudo apt-get install -y lcov
      - run: make check
  together:
    steps:
      - run: |
          sudo apt-get install -y libfoo
          make check-all
  feature:
    steps:
      - run: cargo test --features mount
  vars:
    env:
      HOME: /home/runner
    steps:
      - env:
          ORCH_COW_EXPECT_CLONE: "1"
          TMPDIR: /mnt/xfs/tmp
        run: cargo nextest run
"#;
    let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", ci)]);
    assert_eq!(
        checks(&cfg),
        [
            ("feature", "cargo test --features mount"),
            ("setup", "make check"),
            ("together", "make check-all"),
            (
                "vars",
                "export ORCH_COW_EXPECT_CLONE=1 && cargo nextest run"
            ),
        ]
    );
    for line in [
        "ci.yml, job test: runs mount, which only a VM of its own gives a job",
        "ci.yml, job vars: left out variable TMPDIR, which the sandbox sets",
        "ci.yml, job vars: left out variable HOME, which the sandbox sets",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }
    // A step of only setup commands is dropped without a note
    assert!(!said.contains("job setup:"), "{said}");
}

#[test]
fn only_install_lines_are_setup() {
    let ci = r#"on: push
jobs:
  root-script:
    steps:
      - run: sudo ./scripts/test.sh
  aptos:
    steps:
      - run: aptos move test
  health:
    steps:
      - run: curl -f localhost:8080/health
  gh:
    steps:
      - run: gh workflow view ci.yml
  installer:
    steps:
      - run: curl -fsSL https://example.com/install.sh | sh
      - run: make test
  exits:
    steps:
      - run: |
          exit 0
          make test
  semicolon:
    steps:
      - run: make a; make b
  gated:
    steps:
      - run: make lint
        if: matrix.os == 'linux'
      - run: make test-gated
  echoed:
    steps:
      - run: echo "::group::tests"
      - run: brew install jq
      - run: make echoed
"#;
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Running);
    let checks: Vec<(&str, &str)> = found
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.command.as_str()))
        .collect();
    assert_eq!(
        checks,
        [
            ("aptos", "aptos move test"),
            ("installer", "make test"),
            ("echoed", "make echoed"),
        ],
        "{}",
        found.explain()
    );
    let kept: Vec<(&str, &str)> = not_host(&found)
        .iter()
        .map(|k| (k.job.as_str(), k.why.as_str()))
        .collect();
    assert_eq!(
        kept,
        [
            (
                "root-script",
                "a check would leave out its step `sudo ./scripts/test.sh`, a step that needs root, with `sudo`, `mount`, `mkfs` or `apt`"
            ),
            (
                "health",
                "a check would leave out its step `curl -f localhost:8080/health`, a step with `curl`, `wget`, `gh` or `brew` that is not an install"
            ),
            (
                "gh",
                "a check would leave out its step `gh workflow view ci.yml`, a step with `curl`, `wget`, `gh` or `brew` that is not an install"
            ),
            (
                "exits",
                "a check would leave out its step `exit 0`, a step with shell control flow or a function"
            ),
            (
                "semicolon",
                "a check would leave out its step `make a; make b`, a step with `;`, which GitHub's `bash -e` stops at"
            ),
            (
                "gated",
                "a check would leave out its step `make lint`, a step with `if:` or `continue-on-error`"
            ),
        ]
    );
}

#[test]
fn only_known_actions_are_left_out_without_a_trace() {
    for (uses, nothing) in [
        ("actions/checkout@v6", true),
        ("actions/cache/restore@v4", true),
        ("gradle/actions/setup-gradle@v4", true),
        ("Homebrew/actions/setup-homebrew@master", true),
        ("actions-rs/toolchain@v1", true),
        ("jdx/mise-action@v2", true),
        ("pnpm/action-setup@v4", true),
        ("Swatinem/rust-cache@v2", true),
        ("acme/setup-and-run-e2e@v1", false),
        ("acme/cache@v1", false),
        ("acme/action-setup@v1", false),
        ("./.github/actions/setup-node", false),
        ("docker://alpine:3", false),
        ("EmbarkStudios/cargo-deny-action@v2", false),
    ] {
        assert_eq!(drops_nothing(uses), nothing, "{uses}");
    }
}

#[test]
fn a_step_in_an_unusual_directory_keeps_its_job_on_github() {
    let ci = "on: push\njobs:\n  up:\n    steps:\n      - run: make\n        working-directory: ../other\n      - run: make test\n";
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Running);
    assert!(found.checks.is_empty());
    assert_eq!(
        not_host(&found)[0].why,
        "a check would leave out its step `make`, a step with an unusual working directory"
    );
}

#[test]
fn versions_leave_out_setup_tools() {
    let checks = [Found {
            name: "c".into(),
            command: "({ command -v mkfs.xfs >/dev/null || sudo apt-get install -y xfsprogs; } && truncate -s 8G x && mkfs.xfs x && sudo mount x y) && cargo test".into(),
            from: String::new(),
        }];
    assert_eq!(tool_versions(&checks, &[], |_| true), ["cargo --version"]);
}

#[test]
fn a_pinned_toolchain_is_pinned_when_rustup_has_it() {
    // orch's release.yml and an Anchor project's ci.yml, trimmed
    let ci = r#"on: push
env:
  RUST_VERSION: 1.93.1
jobs:
  check:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          toolchain: "1.94.1"
          components: clippy, rustfmt
      - run: cargo clippy --all-targets -- -D warnings
  prop:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          toolchain: ${{ env.RUST_VERSION }}
      - run: cargo test
  nightly:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          toolchain: nightly-2026-01-01
      - run: cargo build
  file:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
      - run: cargo doc
  target:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          target: wasm32-wasip1
      - run: cargo check
"#;
    let found = detect_on(
        repo(&[(".github/workflows/ci.yml", ci)]).path(),
        &Machine {
            toolchains: vec!["stable".into(), "1.93.1".into(), "1.94.1".into()],
            ..Machine::default()
        },
    );
    let checks: Vec<(&str, &str)> = found
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.command.as_str()))
        .collect();
    assert_eq!(
        checks,
        [
            (
                "check",
                "export RUSTUP_TOOLCHAIN=1.94.1 RUST_VERSION=1.93.1 && cargo clippy --all-targets -- -D warnings"
            ),
            (
                "prop",
                "export RUSTUP_TOOLCHAIN=1.93.1 RUST_VERSION=1.93.1 && cargo test"
            ),
            ("nightly", "export RUST_VERSION=1.93.1 && cargo build"),
            ("file", "export RUST_VERSION=1.93.1 && cargo doc"),
        ]
    );
    assert_eq!(
        found.versions,
        ["rustc +1.94.1 --version", "rustc +1.93.1 --version"]
    );
    let said = found.explain();
    for line in [
        "run on this machine's version, not the workflow's:\n  ci.yml, job nightly",
        "ci.yml, job nightly: needs toolchain nightly-2026-01-01, which rustup does not have here, so the check runs on the default toolchain",
        "ci.yml, job target: needs target wasm32-wasip1",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }

    // Without the toolchain the check runs on the default one, with a warning
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Stopped);
    assert!(
            found.warnings.contains(
                &"ci.yml, job check: needs toolchain 1.94.1, which rustup does not have here, so the check runs on the default toolchain until `rustup toolchain install 1.94.1`".to_string()
            ),
            "{}",
            found.explain()
        );
    assert!(found.checks.iter().any(|c| c.name == "check"));
}

#[test]
fn a_job_its_check_would_run_only_part_of_stays_on_github() {
    // curl's checkdocs.yml, checksrc.yml and linux-old.yml, trimmed
    let ci = r"on: pull_request
jobs:
  proselint:
    steps:
      - uses: actions/checkout@v6
      - run: git ls-files '*.md' -z | xargs -0 -n1 .github/scripts/trimmarkdownheader.pl
      - name: check prose
        run: |
          cat <<JSON > ~/.proselintrc.json
          {}
          JSON
          proselint check --config ~/.proselintrc.json docs
  deny:
    steps:
      - run: cargo build
      - uses: EmbarkStudios/cargo-deny-action@v2
  pytype:
    steps:
      - name: install prereqs
        uses: ./.github/actions/pkg-install
        with:
          pip: pytype
      - run: |
          source ~/venv/bin/activate
          pytype -j auto .
  cached:
    steps:
      - uses: ./.github/actions/cache
      - run: make test
  old:
    runs-on: ubuntu-latest
    container: debian:stretch-slim
    steps:
      - run: make
  boxed:
    container:
      image: alpine:3.24
    steps:
      - run: make check
  plain:
    steps:
      - uses: actions/checkout@v6
      - run: make lint
";
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Running);
    let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(checks, ["plain"], "{}", found.explain());
    let kept: Vec<(&str, &str)> = not_host(&found)
        .iter()
        .map(|k| (k.job.as_str(), k.why.as_str()))
        .collect();
    assert_eq!(
        kept,
        [
            (
                "proselint",
                "a check would leave out its step `check prose`, a step with a heredoc"
            ),
            (
                "deny",
                "a check would leave out its step `EmbarkStudios/cargo-deny-action@v2`, a step that uses an action no command stands for"
            ),
            (
                "pytype",
                "a check would leave out its step `install prereqs`, a step that uses an action no command stands for"
            ),
            // A local action is the repository's own code, whatever its name
            (
                "cached",
                "a check would leave out its step `./.github/actions/cache`, a step that uses an action no command stands for"
            ),
            ("boxed", "runs in container alpine:3.24"),
            ("old", "runs in container debian:stretch-slim"),
        ]
    );
    // Only `old` names a runner act has an image for, the rest stay on GitHub and say why
    let acts: Vec<&str> = found.acts.iter().map(|(k, _)| k.job.as_str()).collect();
    assert_eq!(acts, ["old"]);
    assert!(
        found
            .github
            .iter()
            .all(|k| k.act.as_deref() == Some("its runs-on is not a label act has an image for"))
    );
}

#[test]
fn a_matrix_of_one_combination_is_a_plain_job() {
    // A Python app's ci.yml and git-cow's, trimmed
    let ci = r"on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        python-version: [3.13]
    steps:
      - uses: actions/checkout@v6
      - run: uv run pytest -v --python ${{ matrix.python-version }}
  one:
    strategy:
      matrix:
        include:
          - {os: ubuntu-latest, fs: ext4}
    runs-on: ${{ matrix.os }}
    steps:
      - run: make test FS=${{matrix.fs}}
";
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Running);
    assert!(found.act.is_none() && found.github.is_empty());
    let checks: Vec<(&str, &str)> = found
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.command.as_str()))
        .collect();
    assert_eq!(
        checks,
        [
            ("test", "uv run pytest -v --python 3.13"),
            ("one", "make test FS=ext4")
        ]
    );
    let jobs = workflow::job_runs(ci);
    assert_eq!(jobs[0].check_names, Some(vec!["test (3.13)".to_string()]));

    // Two values are a matrix, and the reason shows them
    let two = ci.replace("[3.13]", "[3.12, 3.13]");
    let found = detect_with(&[(".github/workflows/ci.yml", &two)], Docker::Running);
    assert!(
        found
            .act
            .as_deref()
            .is_some_and(|w| w.contains("runs a matrix (python-version: 3.12, 3.13)")),
        "{:?}",
        found.act
    );
}

// orch's orch-review.yml, trimmed
const REVIEW: &str = r"on:
  push:
jobs:
  review-gate:
    runs-on: ubuntu-latest
    if: github.ref != format('refs/heads/{0}', github.event.repository.default_branch)
    steps:
      - run: ./scripts/lint-frontmatter.sh
";

#[test]
fn a_job_that_runs_only_off_the_base_branch_stays_on_github() {
    let ci = "on: push\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n";
    let found = detect_with(
        &[
            (".github/workflows/ci.yml", ci),
            (".github/workflows/orch-review.yml", REVIEW),
        ],
        Docker::Stopped,
    );
    assert_eq!(found.checks[0].command, "make lint");
    assert_eq!(found.github.len(), 1);
    assert_eq!(found.github[0].job, "review-gate");
    assert_eq!(found.github[0].why, PR_ONLY);

    // In act mode its workflow is not one of the checks
    let matrix = "on: push\njobs:\n  test:\n    strategy:\n      matrix:\n        py: ['3.12', '3.13']\n    steps:\n      - run: make test\n";
    let dir = repo(&[
        (".github/workflows/ci.yml", matrix),
        (".github/workflows/orch-review.yml", REVIEW),
    ]);
    let said = init_on(
        dir.path(),
        &Machine {
            docker: true,
            ..Machine::default()
        },
    )
    .unwrap()
    .unwrap();
    let cfg =
        RepoConfig::parse(&fs::read_to_string(dir.path().join("shipcheck.toml")).unwrap()).unwrap();
    assert_eq!(cfg.workflows, ["ci.yml"]);
    assert!(
        said.contains("orch-review.yml stays on GitHub as it is"),
        "{said}"
    );
}

#[test]
fn a_project_check_a_job_on_github_runs_says_so() {
    // git-cow's ci.yml, trimmed
    let ci = r"on: [push, pull_request]
jobs:
  test:
    strategy:
      matrix:
        include:
          - {os: macos-latest, fs: apfs}
          - {os: ubuntu-latest, fs: xfs}
    runs-on: ${{ matrix.os }}
    steps:
      - run: cargo test
";
    let found = detect_with(
        &[(".github/workflows/ci.yml", ci), ("Cargo.toml", "")],
        Docker::Stopped,
    );
    let said = found.explain();
    assert!(
        said.contains("Cargo.toml: ci.yml, job test runs it on GitHub: `cargo test`"),
        "{said}"
    );
    assert!(
        said.contains("Cargo.toml: no workflow runs it: `cargo clippy"),
        "{said}"
    );
    let dir = repo(&[(".github/workflows/ci.yml", ci)]);
    let said = init(dir.path()).unwrap().unwrap();
    assert!(said.starts_with(NOTHING_LOCAL), "{said}");
    assert!(!said.contains("step 4"), "{said}");
}

#[test]
fn gitleaks_action_becomes_gitleaks_dir() {
    // orch's release.yml secrets job, trimmed
    let ci = r"on: push
jobs:
  secrets:
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: gitleaks/gitleaks-action@v2
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GITLEAKS_CONFIG: .gitleaks.toml
  leaks:
    steps:
      - uses: gitleaks/gitleaks-action@v2
";
    let (cfg, _, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
    assert_eq!(
        checks(&cfg),
        [
            (
                "leaks",
                r#"d=$(mktemp -d "$HOME/scan.XXXXXX") && git archive HEAD | tar -x -C "$d" && gitleaks dir --redact "$d""#
            ),
            (
                "secrets",
                r#"d=$(mktemp -d "$HOME/scan.XXXXXX") && git archive HEAD | tar -x -C "$d" && gitleaks dir --redact --config .gitleaks.toml "$d""#
            ),
        ]
    );
    let found = |command: &str| Found {
        name: "c".into(),
        command: command.into(),
        from: String::new(),
    };
    assert_eq!(
        tool_versions(&[found("gitleaks dir --redact .")], &[], |_| true),
        ["gitleaks --version"]
    );
}

#[test]
fn workflow_env_is_exported_once_per_check() {
    // An Anchor project's ci.yml, trimmed
    let ci = r#"on: push
env:
  RUST_VERSION: 1.93.1
jobs:
  build:
    steps:
      - run: anchor build
  integration:
    needs: build
    env:
      EXTRA: "1"
    steps:
      - run: anchor test
"#;
    let (cfg, text, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
    assert_eq!(
        cfg.host.checks["integration"],
        "export RUST_VERSION=1.93.1 && (anchor build) && export EXTRA=1 && anchor test"
    );
    assert!(
            text.contains("# check uses this machine's tools, which `versions` records: RUST_VERSION\n[host.checks]"),
            "{text}"
        );
}

#[test]
fn hook_is_marked_and_hands_off_to_shipcheck() {
    assert!(PRE_PUSH.starts_with("#!/bin/sh\n"));
    assert!(PRE_PUSH.contains(MARK));
    assert!(PRE_PUSH.contains(HOOK_LINE));
}

#[test]
fn hooks_an_older_install_wrote_are_stale_and_foreign_ones_never() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    let hooks = root.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    assert!(!stale_hooks(root), "no hooks at all are not stale");

    fs::write(hooks.join("pre-push"), PRE_PUSH).unwrap();
    fs::write(hooks.join("post-commit"), POST_COMMIT).unwrap();
    assert!(!stale_hooks(root), "the hooks this version writes");

    // The pre-push of 2026-10-22, before it passed the remote ref
    let old = PRE_PUSH.replace(" \"$remote_ref\"", "");
    fs::write(hooks.join("pre-push"), &old).unwrap();
    assert!(stale_hooks(root));

    fs::write(
        hooks.join("pre-push"),
        "#!/bin/sh\nexec lefthook run pre-push\n",
    )
    .unwrap();
    assert!(!stale_hooks(root), "another hook manager's hook");
    assert!(!stale_hooks(&root.join("nowhere")), "outside a repository");
}

#[test]
fn the_install_gap_names_the_repository_and_the_fix() {
    assert_eq!(gap_line("homelab", Entry::Registered, Hooks::Current), None);
    assert_eq!(
        gap_line("homelab", Entry::Missing, Hooks::Current).unwrap(),
        "homelab is not registered with the daemon, so a PR pushed from elsewhere gets no run, run `shipcheck install` to fix"
    );
    assert_eq!(
        gap_line("homelab", Entry::Registered, Hooks::Stale).unwrap(),
        "homelab has outdated hooks, run `shipcheck install` to fix"
    );
    assert!(
        gap_line("homelab", Entry::Missing, Hooks::Stale)
            .unwrap()
            .contains("not registered with the daemon and has outdated hooks")
    );
}

// Alamofire's ci.yml, and the other ways a workflow picks an Xcode
const XCODE: &str = r#"on: pull_request
jobs:
  env:
    runs-on: macOS-26
    env:
      DEVELOPER_DIR: "/Applications/Xcode 26.6.app/Contents/Developer"
    steps:
      - run: xcodebuild -scheme "Alamofire macOS" build
  select:
    runs-on: macos-15
    steps:
      - run: sudo -E xcode-select -s /Applications/Xcode_26.6.app && swift test
  switch:
    runs-on: macos-15
    steps:
      - run: xcode-select --switch=/Applications/Xcode_26.6.app
      - run: swift build
  githubenv:
    runs-on: macos-15
    steps:
      - run: echo "DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer" >> $GITHUB_ENV
      - run: swift build -v
  action:
    runs-on: macos-15
    steps:
      - uses: maxim-lobanov/setup-xcode@v1
        with:
          xcode-version: '26.6'
      - run: swift test -c debug
  beside:
    runs-on: macos-15
    steps:
      - run: sudo xcode-select -s /Applications/Xcode_26.6.app || true
      - run: swift build --build-tests
  latest:
    runs-on: macos-15
    steps:
      - uses: maxim-lobanov/setup-xcode@v1
        with:
          xcode-version: latest-stable
      - run: xcrun swift build
  tools:
    runs-on: macos-15
    steps:
      - run: sudo xcode-select -s /Library/Developer/CommandLineTools
      - run: swift build
"#;

fn xcode_on(xcodes: &[&str]) -> Detected {
    detect_on(
        repo(&[(".github/workflows/ci.yml", XCODE)]).path(),
        &Machine {
            mac: true,
            xcodes: xcodes
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            ..Machine::default()
        },
    )
}

fn commands_of(found: &Detected) -> Vec<(&str, &str)> {
    found
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.command.as_str()))
        .collect()
}

#[test]
fn a_job_that_picks_two_xcodes_stays_whole() {
    let ci = "on: pull_request\njobs:\n  two:\n    runs-on: macos-latest\n    steps:\n      - run: sudo xcode-select -s /Applications/Xcode_26.6.app\n      - run: swift build\n      - run: sudo xcode-select -s /Applications/Xcode_16.4.app\n      - run: swift test\n";
    let found = detect_on(
        repo(&[(".github/workflows/ci.yml", ci)]).path(),
        &Machine {
            mac: true,
            xcodes: vec![
                "/Applications/Xcode_26.6.app".into(),
                "/Applications/Xcode_16.4.app".into(),
            ],
            ..Machine::default()
        },
    );
    assert!(found.checks.is_empty(), "{}", found.explain());
    assert_eq!(found.github[0].why, "picks more than one Xcode");
}

#[test]
fn a_job_that_needs_xcode_stays_on_github_on_a_machine_without_one() {
    let found = xcode_on(&[]);
    let kept: Vec<(&str, &str)> = found
        .github
        .iter()
        .map(|k| (k.job.as_str(), k.why.as_str()))
        .collect();
    let why = "needs Xcode, which this machine does not have";
    assert_eq!(
        kept,
        [
            "env",
            "select",
            "switch",
            "githubenv",
            "action",
            "beside",
            "latest"
        ]
        .map(|j| (j, why))
    );
    // The Command Line Tools are not an Xcode
    assert_eq!(commands_of(&found), [("tools", "swift build")]);
}

#[test]
fn a_job_that_picks_an_installed_xcode_runs_with_it() {
    let found = xcode_on(&[
        "/Applications/Xcode 26.6.app",
        "/Applications/Xcode_26.6.app",
    ]);
    let spaced = "DEVELOPER_DIR='/Applications/Xcode 26.6.app/Contents/Developer'";
    let dir = "DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer";
    assert_eq!(
        commands_of(&found),
        [
            (
                "env",
                format!("export {spaced} && xcodebuild -scheme \"Alamofire macOS\" build").as_str()
            ),
            ("select", &format!("export {dir} && swift test")),
            ("switch", &format!("export {dir} && swift build")),
            ("githubenv", &format!("export {dir} && swift build -v")),
            ("action", &format!("export {dir} && swift test -c debug")),
            (
                "beside",
                &format!("export {dir} && swift build --build-tests")
            ),
            ("latest", "xcrun swift build"),
            ("tools", "swift build"),
        ]
    );
    // `xcode-select -s <app> || true` only picks the Xcode the check's DEVELOPER_DIR names
    assert!(
        found.github.is_empty(),
        "{:?}",
        found.github.iter().map(|k| &k.why).collect::<Vec<_>>()
    );
    for version in [
        format!("{spaced} xcodebuild -version"),
        format!("{dir} xcodebuild -version"),
        "xcodebuild -version".to_string(),
    ] {
        assert!(
            found.versions.contains(&version),
            "{version}\n{:?}",
            found.versions
        );
    }
}

#[test]
fn a_job_that_picks_another_xcode_runs_with_the_default_one() {
    let found = xcode_on(&["/Applications/Xcode_16.4.app"]);
    assert!(found.github.is_empty());
    let checks = commands_of(&found);
    assert_eq!(
        checks[0],
        ("env", "xcodebuild -scheme \"Alamofire macOS\" build")
    );
    assert_eq!(checks[1], ("select", "swift test"));
    assert!(
            found.skipped.contains(
                &"ci.yml, job env: left out /Applications/Xcode 26.6.app, which this machine does not have, so it runs with the default Xcode"
                    .to_string()
            ),
            "{:?}",
            found.skipped
        );
}

#[test]
fn a_quoted_semicolon_is_part_of_its_word() {
    let ci = "on: pull_request\njobs:\n  quoted:\n    steps:\n      - run: sed -i'' 's;@MainActor;;g' Tests/*.swift\n      - run: swift build\n  bare:\n    steps:\n      - run: make; make test\n  android:\n    steps:\n      - run: sed -i'' 's;@MainActor;;g' Tests/*.swift\n      - uses: skiptools/swift-android-action@v2\n";
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Stopped);
    assert_eq!(
        commands_of(&found),
        [(
            "quoted",
            "sed -i'' 's;@MainActor;;g' Tests/*.swift && swift build"
        )]
    );
    // Its action does the work, so a check of the `sed` alone would pass whatever the code is
    assert!(
        found
            .github
            .iter()
            .any(|k| k.job == "android" && k.why.contains("uses an action no command stands for")),
        "{:?}",
        found.github.iter().map(|k| &k.why).collect::<Vec<_>>()
    );
    assert_eq!(unquoted(r#"a "b;c" 'd;e' f\;g ;"#), "a bc de fg ;");
    assert!(unquoted(r"echo $'\'' ; make test").contains(';'));
    assert!(!checks_nothing("\"./ci/test.sh\""));
}

#[test]
fn a_stays_reason_is_about_its_own_job() {
    let ci = r"on: pull_request
jobs:
  visionOS:
    runs-on: ${{ matrix.runsOn }}
    strategy:
      matrix:
        include:
          - runsOn: self-xcode266
          - runsOn: self-xcode264
    steps:
      - run: xcodebuild test
  linux:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        image: [swift:6.2, swift:6.3]
    steps:
      - run: swift build
";
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Running);
    let why: Vec<&str> = not_host(&found).iter().map(|k| k.why.as_str()).collect();
    assert_eq!(
        why,
        [
            "runs a matrix (runsOn: self-xcode266, self-xcode264) with macOS entries, which act cannot run",
            "runs a matrix (image: swift:6.2, swift:6.3), and host mode runs only this machine's toolchain",
        ]
    );
}

#[test]
fn a_tool_not_on_path_is_named() {
    let checks = [Found {
            name: "build".into(),
            command: "export A=1 && set -o pipefail && eval \"$X\" && exec foo && find . -exec ls {} + && xcodebuild build | xcbeautify && pip install ruff && ruff check".into(),
            from: String::new(),
        }];
    assert_eq!(
        missing_tools(&checks, |t| ["xcodebuild", "pip"].contains(&t)),
        ["build calls xcbeautify"]
    );
}
