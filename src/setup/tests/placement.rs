use super::*;

#[test]
fn a_linux_matrix_runs_the_repository_in_act_with_docker() {
    let release =
        "on:\n  push:\n    branches: [main]\njobs:\n  release:\n    steps:\n      - run: make\n";
    let files = [
        (".github/workflows/CI.yml", AXUM),
        (".github/workflows/release-plz.yml", release),
    ];
    let found = detect_with(&files, Docker::Running);
    assert_eq!(
        found.act.as_deref(),
        Some(
            "CI.yml, job check runs a matrix (workspace: ., examples): host mode tests only this machine's toolchain, so this repository runs in act"
        )
    );
    assert!(found.checks.is_empty() && found.github.is_empty());

    let dir = repo(&files);
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
    assert_eq!(cfg.runner, RunnerKind::Act);
    assert_eq!(cfg.workflows, ["CI.yml"]);
    assert!(
        said.contains("runs a matrix (workspace: ., examples)"),
        "{said}"
    );
}

#[test]
fn without_docker_a_matrix_stays_on_github_and_toolchains_are_left_out() {
    let found = detect_with(&[(".github/workflows/CI.yml", AXUM)], Docker::Stopped);
    assert!(found.act.is_none());
    let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(checks, ["check-docs", "external-types"]);
    let said = found.explain();
    for line in [
        "  CI.yml, job check: runs a matrix (workspace: ., examples) and Docker is not running, so act cannot run it",
        "  CI.yml, job external-types: needs toolchain nightly-2026-03-20",
        "  CI.yml, job test-nightly: needs a toolchain from an expression",
        "  CI.yml, job armv5te-unknown-linux-musleabi: needs target armv5te-unknown-linux-musleabi",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }
    assert_eq!(
        found.github[0].checks.as_deref(),
        Some(&["check (.)".to_string(), "check (examples)".to_string()][..])
    );
}

#[test]
fn a_matrix_with_macos_or_windows_entries_stays_on_github() {
    // flask's tests.yaml and express's ci.yml, trimmed
    let flask = r"on:
  pull_request:
jobs:
  tests:
    name: ${{ matrix.name || matrix.python }}
    runs-on: ${{ matrix.os || 'ubuntu-latest' }}
    strategy:
      matrix:
        include:
          - {python: '3.15'}
          - {python: '3.14'}
          - {name: Windows, python: '3.14', os: windows-latest}
          - {name: Mac, python: '3.14', os: macos-latest}
          - {python: '3.13'}
          - {python: '3.12'}
          - {python: '3.11'}
    steps:
      - uses: astral-sh/setup-uv@v9
        with:
          enable-cache: true
      - run: uv run --locked --no-default-groups --group dev tox run
        env:
          TOX_ENV: ${{ matrix.tox || format('py{0}', matrix.python) }}
  typing:
    runs-on: ubuntu-latest
    steps:
      - uses: astral-sh/setup-uv@v9
      - run: uv run --locked --no-default-groups --group dev tox run -e typing
";
    let express = r"on: [push, pull_request]
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-node@v7
        with:
          cache: npm
      - run: npm install --ignore-scripts --include=dev
      - run: npm run lint
  test:
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, windows-latest]
        node-version: [18, 26]
    name: Node.js ${{ matrix.node-version }} - ${{matrix.os}}
    steps:
      - uses: actions/setup-node@v7
        with:
          node-version: ${{ matrix.node-version }}
      - run: npm install
      - run: npm run test-ci
";
    // Docker cannot help: act runs no macOS or Windows entry
    let found = detect_with(
        &[
            (".github/workflows/tests.yaml", flask),
            (".github/workflows/ci.yml", express),
        ],
        Docker::Running,
    );
    assert!(found.act.is_none());
    let checks: Vec<(&str, &str)> = found
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.command.as_str()))
        .collect();
    assert_eq!(
        checks,
        [
            (
                "lint",
                "npm install --ignore-scripts --include=dev && npm run lint"
            ),
            (
                "typing",
                "uv run --locked --no-default-groups --group dev tox run -e typing"
            ),
        ]
    );
    let said = found.explain();
    for line in [
        "  ci.yml, job test: runs a matrix (node-version: 18, 26) with Windows entries, which act cannot run",
        "  tests.yaml, job tests: runs a matrix (python: 3.15, 3.14, 3.13, 3.12, ...) with macOS and Windows entries, which act cannot run",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }
    assert_eq!(
        found.github[0].checks.as_deref(),
        Some(
            &[
                "Node.js 18 - ubuntu-latest".to_string(),
                "Node.js 26 - ubuntu-latest".to_string(),
                "Node.js 18 - windows-latest".to_string(),
                "Node.js 26 - windows-latest".to_string(),
            ][..]
        )
    );
    // A matrix of `include` entries has names init cannot tell
    assert!(found.github[1].checks.is_none());
}

#[test]
fn windows_and_macos_jobs_stay_on_github() {
    // cobra's test.yml and ripgrep's ci.yml, trimmed
    let cobra = r"on:
  push:
  pull_request:
jobs:
  test-unix:
    strategy:
      matrix:
        platform: [ubuntu, macOS]
        go: [17, 24]
    name: '${{ matrix.platform }} | 1.${{ matrix.go }}.x'
    runs-on: ${{ matrix.platform }}-latest
    steps:
      - run: make richtest
  test-win:
    name: MINGW64
    defaults:
      run:
        shell: msys2 {0}
    runs-on: windows-latest
    steps:
      - shell: bash
        run: git config --global core.autocrlf input
      - run: make test
  mac-only:
    runs-on: macos-14
    steps:
      - run: make test
";
    let ripgrep = r"on:
  pull_request:
jobs:
  wasm:
    runs-on: ubuntu-latest
    steps:
    - uses: dtolnay/rust-toolchain@master
      with:
        toolchain: stable
    - run: rustup target add wasm32-wasip1
    - run: cargo build --verbose
  rustfmt:
    runs-on: ubuntu-latest
    steps:
    - uses: dtolnay/rust-toolchain@master
      with:
        toolchain: stable
        components: rustfmt
    - run: cargo fmt --all --check
";
    let files = [
        (".github/workflows/test.yml", cobra),
        (".github/workflows/ci.yml", ripgrep),
    ];
    let found = detect_with(&files, Docker::Stopped);
    let kept: Vec<(&str, &str)> = found
        .github
        .iter()
        .map(|k| (k.job.as_str(), k.why.as_str()))
        .collect();
    assert_eq!(
        kept,
        [
            (
                "test-unix",
                "runs a matrix (go: 17, 24) with macOS entries, which act cannot run"
            ),
            (
                "test-win",
                "runs on Windows, which shipcheck does not support yet"
            ),
            ("mac-only", "runs on macOS, and this machine is not a Mac"),
        ]
    );
    assert_eq!(
        found.github[1].checks.as_deref(),
        Some(&["MINGW64".to_string()][..])
    );
    assert_eq!(
        found.github[0].checks.as_ref().unwrap()[1],
        "ubuntu | 1.24.x"
    );
    let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(checks, ["rustfmt"]);
    assert!(
        found
            .explain()
            .contains("  ci.yml, job wasm: needs target wasm32-wasip1"),
        "{}",
        found.explain()
    );

    // On a Mac, host mode runs the macOS job
    let dir = repo(&files);
    let mac = detect_on(
        dir.path(),
        &Machine {
            docker: false,
            mac: true,
            ..Machine::default()
        },
    );
    assert!(mac.checks.iter().any(|c| c.name == "mac-only"));
    assert!(!mac.github.iter().any(|k| k.job == "mac-only"));
}

#[test]
fn what_a_job_on_github_needs_stays_there_too() {
    let ci = r#"on: pull_request
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: make lint
  tests:
    needs: [lint]
    runs-on: ubuntu-latest
    strategy:
      matrix:
        python: ["3.12", "3.13"]
    steps:
      - run: make test
  docs:
    runs-on: ubuntu-latest
    steps:
      - run: make docs
"#;
    let found = detect_with(&[(".github/workflows/ci.yml", ci)], Docker::Stopped);
    let kept: Vec<(&str, &str)> = found
        .github
        .iter()
        .map(|k| (k.job.as_str(), k.why.as_str()))
        .collect();
    assert_eq!(
        kept,
        [
            (
                "tests",
                "runs a matrix (python: 3.12, 3.13) and Docker is not running, so act cannot run it"
            ),
            ("lint", "job tests stays on GitHub and needs it"),
        ]
    );
    let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(checks, ["docs"]);
}

#[test]
fn a_matrix_with_no_run_step_does_not_make_it_act() {
    // express's codeql.yml, trimmed
    let codeql = "on: pull_request\njobs:\n  analyze:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        language: [javascript, actions]\n    steps:\n      - uses: github/codeql-action/init@v4\n";
    let ci = "on: pull_request\njobs:\n  lint:\n    steps:\n      - run: make lint\n";
    let found = detect_with(
        &[
            (".github/workflows/codeql.yml", codeql),
            (".github/workflows/ci.yml", ci),
        ],
        Docker::Running,
    );
    assert!(found.act.is_none());
    // Its only step is an action no command stands for
    assert_eq!(not_host(&found)[0].job, "analyze");
    assert_eq!(found.checks[0].command, "make lint");
}

#[test]
fn setup_actions_that_install_keep_their_install() {
    // vite's ci.yml lint and rails' ci.yml, trimmed
    let ci = r"on: pull_request
jobs:
  lint:
    steps:
      - uses: pnpm/setup@a0881977cd8ea73b1ed212236c99ca10dcc80cf4 # v3.1.1
        with:
          runtime: node@24
          cache: true
      - run: pnpm run build
  no-install:
    steps:
      - uses: pnpm/setup@v3
        with:
          install: false
      - run: pnpm run build2
  action-setup:
    steps:
      - uses: pnpm/action-setup@v4
        with:
          run_install: true
      - run: pnpm test
  ruby:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          bundler-cache: true
      - run: bundle exec rake
  node:
    steps:
      - uses: actions/setup-node@v7
        with:
          cache: npm
      - uses: astral-sh/setup-uv@v9
        with:
          enable-cache: true
      - run: npm test
";
    let (cfg, _, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
    assert_eq!(
        checks(&cfg),
        [
            ("action-setup", "pnpm install && pnpm test"),
            ("lint", "pnpm install && pnpm run build"),
            ("no-install", "pnpm run build2"),
            ("node", "npm test"),
            ("ruby", "bundle install && bundle exec rake"),
        ]
    );
}

#[test]
fn a_job_runs_on_the_workflows_ruby_or_stays_without_one() {
    let ci = r"
on: pull_request
jobs:
  latest:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          ruby-version: ruby
          bundler-cache: true
      - run: bundle exec rake
  same:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          ruby-version: '4.0'
      - run: bundle exec rake a
  old:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          ruby-version: 2.7
      - run: bundle exec rake b
  jruby:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          ruby-version: jruby
      - run: bundle exec rake c
  file:
    steps:
      - uses: ruby/setup-ruby@v1
      - run: bundle exec rake d
";
    let rubies = [Ruby {
        engine: "ruby".into(),
        version: "2.7.8".into(),
        root: "/rubies/2.7.8".into(),
    }];
    let d = detect_on(
        repo(&[
            (".github/workflows/ci.yml", ci),
            (".ruby-version", "ruby-3.4.1\n"),
        ])
        .path(),
        &Machine {
            ruby: Some("4.0.9".into()),
            rubies: rubies.to_vec(),
            ..Machine::default()
        },
    );
    let checks: Vec<(&str, &str)> = d
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.command.as_str()))
        .collect();
    assert_eq!(
        checks,
        [
            ("latest", "bundle install && bundle exec rake"),
            ("same", "bundle exec rake a"),
            (
                "old",
                "export RUBY_ROOT=/rubies/2.7.8 PATH=/rubies/2.7.8/bin:\"$PATH\" && bundle exec rake b"
            ),
            ("file", "bundle exec rake d"),
        ]
    );
    assert!(
        d.skipped
            .iter()
            .any(|s| s == "ci.yml, job jruby: needs jruby, which this machine does not have"),
        "{:?}",
        d.skipped
    );
    // A check on this machine's Ruby is a warning, not a job left out
    assert_eq!(
        d.warnings,
        [
            "ci.yml, job file: runs on Ruby 4.0.9, which differs from the Ruby 3.4.1 its workflow sets up"
        ]
    );

    // Each Ruby a check runs on is in the key
    for version in ["/rubies/2.7.8/bin/ruby --version", "ruby --version"] {
        assert!(
            d.versions.iter().any(|v| v == version),
            "{version}: {:?}",
            d.versions
        );
    }
}

/// What `init` finds in these files on a Linux machine with or without Docker
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one case per kind of job, read as one table"
)]
fn a_job_no_host_check_runs_as_github_does_goes_to_act_unless_act_differs() {
    let job = |id: &str, extra: &str, steps: &str| {
        format!("  {id}:\n    runs-on: ubuntu-latest\n{extra}    steps:\n{steps}")
    };
    let heredoc = "      - run: |\n          cat <<EOF > a\n          x\n          EOF\n          make test\n";
    let ci = [
            "on: pull_request\njobs:\n".to_string(),
            job("lint", "", "      - run: make lint\n"),
            job("heredoc", "", heredoc),
            job("e2e", "", "      - run: docker build .\n"),
            job("podman", "", "      - run: sudo podman build .\n"),
            job("userns", "", "      - run: sudo apparmor_parser -r /etc/apparmor.d/bwrap\n      - run: make test\n"),
            job("words", "", "      - run: echo mount the disk && make check\n      - uses: some/action@v1\n"),
            job("wasm", "", "      - uses: dtolnay/rust-toolchain@stable\n        with:\n          targets: wasm32-wasip1\n      - run: cargo check\n"),
            job("pr", "    if: github.event_name == 'pull_request'\n", heredoc),
            job("owner", "    if: github.repository_owner == 'acme'\n", heredoc),
            job("changed", "", "      - uses: some/lint@v1\n        with:\n          base: ${{ github.event.pull_request.base.sha }}\n"),
            job("codeql", "", "      - uses: github/codeql-action/analyze@v4\n"),
            job("newer", "", heredoc).replace("ubuntu-latest", "ubuntu-26.04-arm"),
            job("matrix", "", heredoc).replace("ubuntu-latest", "${{ matrix.os }}"),
            "  mac:\n    runs-on: macos-latest\n    steps:\n      - uses: some/action@v1\n".into(),
            "  after:\n    needs: mac\n    runs-on: ubuntu-latest\n    steps:\n      - uses: some/action@v1\n".into(),
            // A host check of its own, kept on GitHub only because a macOS job needs it
            "  prep:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make prep\n".into(),
            "  mac2:\n    needs: prep\n    runs-on: macos-latest\n    steps:\n      - run: make mac\n".into(),
            job("evname", "", "      - run: if [ \"$GITHUB_EVENT_NAME\" = pull_request ]; then make lint-changed; fi\n"),
            job("baseref", "", "      - run: git diff origin/$GITHUB_BASE_REF\n      - uses: some/action@v1\n"),
            job("evpath", "", "      - run: jq .pull_request \"$GITHUB_EVENT_PATH\"\n      - uses: some/action@v1\n"),
            job("secret", "", "      - if: secrets.TOKEN != ''\n        run: make upload\n      - uses: some/action@v1\n"),
            job("var", "", "      - if: vars.RUN_SLOW == 'true'\n        run: make slow\n      - uses: some/action@v1\n"),
            job("ref", "", "      - if: github.ref != 'refs/heads/main'\n        run: make pr\n      - uses: some/action@v1\n"),
            job("changes", "", "      - uses: tj-actions/changed-files@v46\n"),
            job("newissues", "", "      - uses: golangci/golangci-lint-action@v8\n        with:\n          only-new-issues: true\n"),
            job("envwrap", "", "      - run: env X=1 podman build .\n"),
            job("shellc", "", "      - run: bash -c 'docker build .'\n"),
            // Its action.sh runs `docker run`, which the workflow never shows
            job("zizmor", "", "      - uses: zizmorcore/zizmor-action@6fc4b006235f201fdab3722e17240ab420d580e5 # v0.6.1\n"),
            job("own", "", "      - run: ./scripts/service start && make check\n      - uses: some/action@v1\n"),
        ]
        .concat();
    let place = |docker: bool, colima: bool| {
        detect_on(
            repo(&[(".github/workflows/ci.yml", &ci)]).path(),
            &Machine {
                docker,
                colima,
                ..Machine::default()
            },
        )
    };
    let why = |d: &Detected, id: &str| {
        d.github
            .iter()
            .find(|k| k.job == id)
            .and_then(|k| k.act.clone())
            .unwrap_or_default()
    };

    // Without Docker nothing changes: what no host check runs stays on GitHub, as it said before
    let found = place(false, false);
    assert!(found.acts.is_empty());
    assert!(found.github.iter().all(|k| k.act.is_none()));
    assert_eq!(found.checks[0].name, "lint");

    let found = place(true, true);
    let acts: Vec<(&str, Engine)> = found
        .acts
        .iter()
        .map(|(k, e)| (k.job.as_str(), *e))
        .collect();
    assert_eq!(
        acts,
        // A target this machine lacks is no host check, act sets it up as GitHub does
        [
            ("heredoc", Engine::Docker),
            ("words", Engine::Docker),
            // `./scripts/service` is the repository's own script, not the system's `service`
            ("own", Engine::Docker),
            ("wasm", Engine::Docker)
        ]
    );
    for (id, reason) in [
        (
            "e2e",
            "it runs docker, and colima could run it, but colima reaches this machine's loopback, so it is opt-in",
        ),
        ("podman", "it runs podman, and colima could run it"),
        ("userns", "it runs apparmor_parser"),
        (
            "pr",
            "its `if: github.event_name == 'pull_request'` can run otherwise under act",
        ),
        ("owner", "its `if: github.repository_owner == 'acme'`"),
        ("changed", "it reads `github.event`"),
        (
            "codeql",
            "it reports to GitHub or publishes with github/codeql-action/analyze@v4",
        ),
        ("newer", "act has no image for its label ubuntu-26.04-arm"),
        ("matrix", "its runs-on is not a label act has an image for"),
        ("after", "job mac it needs:"),
        ("evname", "it reads `GITHUB_EVENT_NAME`"),
        ("baseref", "it reads `GITHUB_BASE_REF`"),
        ("evpath", "it reads `GITHUB_EVENT_PATH`"),
        (
            "secret",
            "a step's `if: secrets.TOKEN != ''` reads `secrets.`",
        ),
        ("var", "reads `vars.`"),
        ("ref", "reads `github.ref`"),
        ("changes", "tj-actions/changed-files@v46 reads the PR"),
        ("newissues", "golangci/golangci-lint-action@v8 reads the PR"),
        ("envwrap", "it runs podman"),
        ("shellc", "it runs docker"),
        ("zizmor", "it runs docker"),
    ] {
        assert!(
            why(&found, id).contains(reason),
            "{id}: {}",
            why(&found, id)
        );
    }

    // A job a macOS job needs stays with it, though act could run it alone
    let prep = found
        .github
        .iter()
        .find(|k| k.job == "prep")
        .expect("prep stays");
    assert_eq!(prep.why, "job mac2 stays on GitHub and needs it");

    // Without colima a VM job says only a VM runs it, and init never writes colima unasked
    assert!(why(&place(true, false), "e2e").ends_with("only a VM runs it"));
    let text = config_text(&found);
    assert!(
        text.contains("\n[jobs.\"ci.yml/heredoc\"]\nrunner = \"act\"\n"),
        "{text}"
    );
    assert!(!text.contains("colima"), "{text}");
    // Before any table, so it is the config's own `network`
    assert!(text.contains("network = \"internet\"\n\n[host]"), "{text}");
    assert!(
        found
            .explain()
            .contains("  ci.yml, job heredoc: a check would leave out its step"),
        "{}",
        found.explain()
    );
    assert!(
        found
            .explain()
            .contains(", and act cannot run it either: it runs docker"),
        "{}",
        found.explain()
    );

    // What init writes runs those jobs with act, beside the host check
    let cfg = RepoConfig::parse(&text).unwrap();
    cfg.validate(&["ci.yml".to_string()].into()).unwrap();
    let jobs = run::expected_jobs(&cfg, |_| Some(ci.clone())).unwrap();
    assert_eq!(
        jobs["ci.yml"],
        ["heredoc", "own", "wasm", "words"].map(String::from).into()
    );
    assert_eq!(jobs[host::HOST], ["lint"].map(String::from).into());

    // A colima job the owner chose needs no network line, which covers Docker's jobs only
    let mut vm = place(true, true);
    vm.acts = vec![(vm.acts.remove(0).0, Engine::Colima)];
    let text = config_text(&vm);
    assert!(
        text.contains("engine = \"colima\"") && !text.contains("network"),
        "{text}"
    );
}

#[test]
fn a_job_that_reads_what_act_lacks_anywhere_stays_on_github() {
    let heredoc = "      - run: |\n          cat <<EOF > a\n          x\n          EOF\n          make test\n";
    let wf = |env: &str, extra: &str, steps: &str| {
        format!(
            "on: pull_request\n{env}jobs:\n  j:\n    runs-on: ubuntu-latest\n{extra}    steps:\n{steps}{heredoc}"
        )
    };
    let run = |r: &str| wf("", "", &format!("      - run: {r}\n"));
    for (case, source, reason) in [
        ("plain", wf("", "", ""), None),
        (
            "wfenv",
            wf(
                "env:\n  PR: ${{ github.event.pull_request.number }}\n",
                "",
                "",
            ),
            Some("`github.event`"),
        ),
        (
            "varenv",
            wf("", "    env:\n      X: ${{ vars.Y }}\n", ""),
            Some("`vars.`"),
        ),
        (
            "secretwith",
            wf(
                "",
                "",
                "      - uses: some/action@v1\n        with:\n          token: ${{ secrets.TOKEN }}\n",
            ),
            Some("`secrets.`"),
        ),
        (
            "refrun",
            run("echo ${{ github.ref }}"),
            Some("`github.ref`"),
        ),
        (
            "envref",
            run("test \"$GITHUB_REF\" = x"),
            Some("`GITHUB_REF`"),
        ),
        (
            "local",
            wf("", "", "      - uses: ./.github/actions/x\n"),
            Some("local action ./.github/actions/x"),
        ),
        (
            "tojson",
            run("echo '${{ toJSON(github) }}'"),
            Some("`tojson(github)`"),
        ),
        (
            "case",
            run("echo ${{ github.EVENT_NAME }}"),
            Some("`github.event`"),
        ),
    ] {
        let found = detect_with(&[(".github/workflows/ci.yml", &source)], Docker::Running);
        let act = found
            .github
            .iter()
            .find(|k| k.job == "j")
            .and_then(|k| k.act.clone());

        match reason {
            None => assert!(
                found.acts.iter().any(|(k, _)| k.job == "j"),
                "{case}: {}",
                found.explain()
            ),
            Some(r) => assert!(
                act.as_deref().is_some_and(|a| a.contains(r)),
                "{case}: {act:?}\n{source}"
            ),
        }
    }
}

#[test]
fn act_running_a_repository_whole_still_holds_back_what_it_would_run_otherwise() {
    let ci = "on: pull_request\njobs:\n  test:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        node: [20, 22]\n    steps:\n      - run: npm test\n";
    let pr = "on: pull_request\njobs:\n  changed:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: tj-actions/changed-files@v46\n  title:\n    runs-on: ubuntu-latest\n    steps:\n      - run: test -n \"${{ github.event.pull_request.title }}\"\n";
    let dir = repo(&[
        (".github/workflows/ci.yml", ci),
        (".github/workflows/pr.yml", pr),
    ]);
    let machine = Machine {
        docker: true,
        ..Machine::default()
    };
    let (text, said) = propose(dir.path(), &machine).unwrap();
    let text = text.unwrap();
    let cfg = RepoConfig::parse(&text).unwrap();
    assert_eq!(cfg.workflows, ["ci.yml"], "{said}");
    // Its jobs download what their actions and steps set up
    assert_eq!(cfg.network, Network::Internet, "{text}");
    for line in [
        "pr.yml stays on GitHub as it is, since act runs a workflow whole: job changed: tj-actions/changed-files@v46 reads the PR",
        "pr.yml stays on GitHub as it is, since act runs a workflow whole: job title: it reads `github.event`",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }

    // With only those, init names why each job stays and what runs on no PR
    let release = "on:\n  push:\n    tags: ['v*']\njobs:\n  publish:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make release\n";
    // A matrix sends the repository to act, and a job beside it reading the PR keeps their workflow on GitHub
    let both = format!(
        "{ci}  title:\n    runs-on: ubuntu-latest\n    steps:\n      - run: test -n \"${{{{ github.event.pull_request.title }}}}\"\n"
    );
    let dir = repo(&[
        (".github/workflows/pr.yml", &both),
        (".github/workflows/release.yml", release),
    ]);
    let (text, said) = propose(dir.path(), &machine).unwrap();
    assert!(text.is_none(), "{said}");
    for line in [
        NOTHING_LOCAL,
        "\n  pr.yml, job title: it reads `github.event`",
        "\n  release.yml: it runs on no PR or branch push",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }
}

#[test]
fn a_tool_that_reads_the_pr_event_itself_stays_on_github() {
    let job = |id: &str, step: &str| {
        format!("  {id}:\n    runs-on: ubuntu-latest\n    steps:\n      - run: {step}\n")
    };
    let jobs = [
        ("danger", "npx danger ci"),
        ("dlx", "yarn dlx danger@11 ci"),
        ("yarn", "yarn danger ci"),
        ("yarnrun", "yarn run danger ci"),
        ("pnpm", "pnpm danger ci"),
        ("filter", "pnpm --filter web exec danger ci"),
        ("bunx", "bun x danger ci"),
        ("swift", "swift run danger-swift ci"),
        ("mint", "mint run danger/swift danger-swift ci"),
        (
            "gorun",
            "go run github.com/reviewdog/reviewdog/cmd/reviewdog@latest -reporter=github-pr-check",
        ),
        ("review", "reviewdog -reporter=github-pr-review -f=eslint"),
        ("npmrun", "npm run danger"),
        ("make", "make danger"),
        ("just", "just danger"),
        ("pnpmroot", "pnpm -w danger ci"),
        ("npmws", "npm -w web exec danger ci"),
        ("silent", "npm run --silent danger"),
        ("yarnws", "yarn workspace web danger ci"),
        ("chain", "npm run a"),
        ("makemake", "make outer"),
        ("makedefault", "make"),
        ("justdefault", "just"),
        ("local", "npx danger local"),
        ("reviewlocal", "reviewdog -reporter=local -f=eslint"),
        ("plain", "./scripts/lint.sh"),
    ];
    let ci = format!(
        "on: pull_request\njobs:\n{}",
        jobs.iter()
            .map(|(id, step)| job(id, step))
            .collect::<String>()
    );
    // With Docker too, so act is no way around it
    let found = detect_with(
        &[
            (".github/workflows/ci.yml", &ci),
            ("scripts/lint.sh", "lint .\n"),
            (
                "package.json",
                "{\"scripts\": {\"danger\": \"danger ci\", \"a\": \"npm run b\", \"b\": \"danger ci\"}}\n",
            ),
            // The first rule past a special target and assignments is the default
            (
                "Makefile",
                ".PHONY: danger outer\nX := 1\nY ?= 2\ndanger:\n\t@npx danger ci\n\nouter:\n\tmake danger\n",
            ),
            (
                "justfile",
                "set shell := [\"bash\", \"-c\"]\nexport A := \"b\"\ndanger:\n    yarn danger ci\n",
            ),
        ],
        Docker::Running,
    );
    // A local run reads no PR, so those two are checks
    let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
    for id in ["local", "reviewlocal", "plain"] {
        assert!(checks.contains(&id), "{id}: {}", found.explain());
    }
    assert!(found.acts.is_empty(), "{}", found.explain());
    for (id, tool) in [
        ("danger", "danger"),
        ("dlx", "danger"),
        ("yarn", "danger"),
        ("yarnrun", "danger"),
        ("pnpm", "danger"),
        ("filter", "danger"),
        ("bunx", "danger"),
        ("swift", "danger-swift"),
        ("mint", "danger-swift"),
        ("gorun", "reviewdog"),
        ("review", "reviewdog"),
        ("npmrun", "danger"),
        ("make", "danger"),
        ("just", "danger"),
        ("pnpmroot", "danger"),
        ("npmws", "danger"),
        ("silent", "danger"),
        ("yarnws", "danger"),
        ("chain", "danger"),
        ("makemake", "danger"),
        ("makedefault", "danger"),
        ("justdefault", "danger"),
    ] {
        let why = found
            .github
            .iter()
            .find(|k| k.job == id)
            .map_or_default(|k| k.why.clone());
        assert_eq!(
            why,
            format!(
                "it runs {tool}, which reads the PR's event itself, and only GitHub's run of a PR has it"
            ),
            "{id}"
        );
    }
}

#[test]
fn a_check_of_a_script_that_reads_the_pr_or_a_diff_from_head_stays_on_github() {
    let job = |id: &str, step: &str| {
        format!("  {id}:\n    runs-on: ubuntu-latest\n    steps:\n      - run: {step}\n")
    };
    let jobs = [
        ("changed", "./scripts/lint-changed.sh".to_string()),
        ("py", "python scripts/changed.py".into()),
        ("node", "node scripts/changed.js".into()),
        ("cdrun", "cd tools && ./lint-changed.sh".into()),
        (
            "wd",
            "./lint-changed.sh\n        working-directory: tools".into(),
        ),
        ("mk", "make lint-changed".into()),
        ("npmrun", "npm run lint:changed".into()),
        ("last", "git diff HEAD^ --check".into()),
        ("head", "./scripts/lint.sh --since HEAD".into()),
        (
            "logone",
            "git log -1 --format=%B | grep -q skip-lint || ./scripts/lint.sh".into(),
        ),
        ("plain", "./scripts/lint.sh".into()),
    ];
    let ci = format!(
        "on: pull_request\njobs:\n{}",
        jobs.iter()
            .map(|(id, step)| job(id, step))
            .collect::<String>()
    );
    let found = detect_with(
        &[
            (".github/workflows/ci.yml", &ci),
            (
                "scripts/lint-changed.sh",
                "git diff origin/$GITHUB_BASE_REF --name-only | xargs lint\n",
            ),
            (
                "scripts/changed.py",
                "import os\nprint(os.environ['GITHUB_BASE_REF'])\n",
            ),
            (
                "scripts/changed.js",
                "console.log(process.env.GITHUB_HEAD_REF)\n",
            ),
            (
                "tools/lint-changed.sh",
                "git diff origin/$GITHUB_BASE_REF\n",
            ),
            (
                "Makefile",
                "lint-changed:\n\tgit diff origin/$$GITHUB_BASE_REF\n",
            ),
            (
                "package.json",
                "{\"scripts\": {\"lint:changed\": \"git diff origin/$GITHUB_BASE_REF\"}}\n",
            ),
            ("scripts/lint.sh", "lint .\n"),
        ],
        Docker::Stopped,
    );
    let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(checks, ["plain"], "{}", found.explain());
    let why = |id: &str| {
        found
            .github
            .iter()
            .find(|k| k.job == id)
            .map_or_default(|k| k.why.clone())
    };
    for (id, file, what) in [
        ("changed", "scripts/lint-changed.sh", "GITHUB_BASE_REF"),
        ("py", "scripts/changed.py", "GITHUB_BASE_REF"),
        ("node", "scripts/changed.js", "GITHUB_HEAD_REF"),
        ("cdrun", "tools/lint-changed.sh", "GITHUB_BASE_REF"),
        ("wd", "tools/lint-changed.sh", "GITHUB_BASE_REF"),
        ("mk", "Makefile", "GITHUB_BASE_REF"),
        ("npmrun", "package.json", "GITHUB_BASE_REF"),
    ] {
        assert_eq!(
            why(id),
            format!("its file {file} reads `{what}`, which only GitHub's run of a PR has"),
            "{id}"
        );
    }
    for id in ["last", "head", "logone"] {
        assert!(
            why(id).starts_with("it reads HEAD, which on a PR is GitHub's merge commit"),
            "{id}: {}",
            why(id)
        );
    }
}
