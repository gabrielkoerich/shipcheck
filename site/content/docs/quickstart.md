+++
title = "Quickstart"
description = "Set up a repository once, then each clone once, then work as usual."
weight = 20
+++

## Setup

1. Install the binary: `brew install gabrielkoerich/tap/shipcheck`, or `curl -fsSL https://shipcheck.run/install.sh | sh` ([install](@/docs/install.md)).
2. Run `shipcheck init` in a clone of the repository, on `main`.
3. Commit the `shipcheck.toml` it writes and merge it to `main`.
4. Install the [shipcheck App](https://github.com/apps/shipcheck-run) on the repository. `init` prints this link.

Each other clone then needs `shipcheck install` once. The sections below say what each step does.

## For the repository owner, once

In a clone of the repository, on `main`:

```sh
shipcheck init
```

It reads your workflows and project files and asks before each step. Press Enter to take the default, or run `shipcheck init --yes` to take every default without questions.

### 1. Checks

`init` turns each job of a workflow that GitHub runs for feature branches into a check named like the job.

- The job's `run` steps are joined by `&&`.
- A job with `needs` runs the jobs it needs first.
- Package installs into the checkout, such as `npm ci`, `bun install --frozen-lockfile` or `uv sync`, stay in the check.
- The workflow's and the job's `env` are exported first, and a step's `env` only in that step.
- Commands that set up the machine, such as `apt`, `sudo`, `curl`, `brew`, `rustup`, `pip install`, `go install`, `git config` or `gh`, are left out. A step that needs root besides other commands, such as one that makes and mounts a volume, is left out whole, and so is a `TMPDIR` or `HOME` value, since the sandbox sets both.
- Steps with a GitHub expression, also in their `env`, `if:` or `continue-on-error` are left out, and so are multi-line steps with control flow.
- A setup action that installs into the checkout keeps its install: `pnpm/setup` (unless `install: false`) and `pnpm/action-setup` with `run_install: true` add `pnpm install`, `ruby/setup-ruby` with `bundler-cache: true` adds `bundle install`. `setup-node` and `setup-uv` caches install nothing. `gitleaks/gitleaks-action` becomes `gitleaks dir --redact .`, which scans the tree. A history scan stays on GitHub.
- Deploy, publish and release jobs are left out, and so are jobs that only install, move files or run git, jobs that download artifacts from a job with no check, and jobs that need a Rust target (`rustup target add`, `targets`). A pinned Rust toolchain, such as `toolchain: "1.94.1"` for `dtolnay/rust-toolchain` or `actions-rust-lang/setup-rust-toolchain`, is pinned with `RUSTUP_TOOLCHAIN` when rustup has it here, else the job is left out with `needs toolchain 1.94.1`. A `rust-toolchain.toml` needs nothing, since rustup reads it.
- Matrix jobs, Windows jobs and, off a Mac, macOS jobs stay on GitHub, or a Linux matrix makes the repository use act. A matrix of one combination, such as `python-version: [3.13]`, is a plain job. A job whose `if:` depends on the PR or the default branch, such as a review gate, stays on GitHub with its workflow as it is. [Which jobs run where](@/docs/how-it-works.md#which-jobs-run-where) has the table.

Only when no job gives a check, a `package.json` `test` or `lint` script, a `Cargo.toml`, a `pyproject.toml` or a `go.mod` gives the checks they imply.

It prints each check with the file it came from, and what it left out and why. Keep them all, or pick them one by one. They run natively in a sandbox:

```toml
runner = "host"

[host]
versions = ["cargo --version", "rustc --version"]

[host.checks]
test = "cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test"
service = "cd service && bun run test"
```

shipcheck has two [modes](@/docs/config.md#modes). `init` writes host mode when it finds checks, and act mode when a job runs a Linux matrix and Docker is running, since host mode tests only this machine's toolchain:

- Host mode runs your commands natively in an OS sandbox, each in a warm copy-on-write clone. It is fast and uses the tools on your machine.
- act mode, `runner = "act"`, runs your GitHub Actions workflows in containers. It is the closest to GitHub's runners and slower to start. A `shipcheck.toml` with no `runner` line runs act mode.

### 2. GitHub App

It prints the install link of the [shipcheck App](https://github.com/apps/shipcheck-run), which posts the checks, and offers to open it in the browser. Install it on the repository.

At the end, `init` signs this machine in if it cannot post yet ([post the checks](@/docs/install.md#post-the-checks)). With `SHIPCHECK_APP_ID` set, it names [your own App](@/docs/install.md#your-own-app) instead.

### 3. Hosted Actions only on `main`

For each workflow that GitHub runs for feature branches, it shows a diff that removes `pull_request` and limits `push` to `branches: [main]`, your default branch. A `push` already limited to branches such as `[main, stable]` stays as it is.

- It writes the change only if you say yes.
- Comments and other triggers stay.
- The default is yes when a shipcheck check covers the workflow. Otherwise it is no, since that workflow's jobs would then run nowhere before a merge.
- A workflow with a job that stays on GitHub keeps its triggers. Each job shipcheck runs gets `if: github.ref == 'refs/heads/main'` instead, so on a PR GitHub runs only the jobs that stay ([which jobs run where](@/docs/how-it-works.md#which-jobs-run-where)).

### 4. Branch protection

It prints one check name per job, such as `shipcheck/test`, and GitHub's own check names of the jobs that stay on GitHub. It can create a ruleset on the default branch, with your own `gh` login, that requires each of them, from the App or from GitHub Actions, with branches up to date.

The default is no. Turn it on once a first PR shows the job checks. A required check that nobody posts blocks every PR, so update the ruleset when a job is added, renamed or removed. [Require the checks](@/docs/branch-protection.md) has the manual steps.

### 5. Hooks

It writes the agent instructions, installs the git hooks for this clone, removes the agent hooks an older version wrote, and starts the daemon, as `shipcheck install` does below.

### Other cases

- When `shipcheck.toml` already exists, `init` leaves it as it is.
- Without a terminal, as for an agent or a script, it writes every check it finds and the agent instructions, installs the hooks and starts the daemon. It asks nothing, and offers no App, workflow or ruleset step.
- When every job stays on GitHub, it writes no `shipcheck.toml`, says why, and stops: no App, workflow or ruleset step, no agent instructions, hooks or daemon.
- When it finds no check at all, it writes `runner = "act"` with every workflow in `workflows`, run in containers, and `shard_tests = true`. Remove the workflows that must not run on a laptop, such as deploys. [Modes](@/docs/config.md#modes) compares the two.

Add `on = ["push", "commit"]` to start runs on each commit too.

Commit it and merge it to `main`. shipcheck reads this file and the workflows only from `main`, so a PR cannot change what it must pass. [Every setting](@/docs/config.md).

Your workflows keep running on GitHub on pushes to `main`, as a backstop after each merge.

## For each teammate, once per clone

```sh
shipcheck install
```

It installs two hooks: pre-push, and post-commit for repositories that run on commits. It also starts the daemon as a background service, a launchd agent on macOS or a systemd user unit on Linux, and adds this repository to it. Set `SHIPCHECK_NO_DAEMON=1` to skip that on one machine. Git never installs hooks on clone, so each clone needs this one step. To make it automatic, run it from the repository's usual setup: the `prepare` script in `package.json`, a `just setup` recipe, or a `mise` task.

If the repository already uses husky, lefthook or another hook manager, `shipcheck install` leaves its hooks alone and prints the line to add to them.

When the machine cannot post yet, `install` signs it in: in a terminal it prints a code to enter at github.com/login/device. Without a terminal it prints one line and goes on.

## For an agent machine

1. [Install the binary](@/docs/install.md).
2. Run `shipcheck install` in each clone. In a terminal, it signs the machine in if it cannot post yet.

A machine with nobody at its terminal, such as a VM, gets a token instead: run `shipcheck token --name <machine>` where you can open a browser and set the output as `SHIPCHECK_TOKEN` there ([unattended machines](@/docs/install.md#unattended-machines)).

## Every day

Commit and push as usual. Nothing waits on shipcheck:

- Each commit or push starts a run in the background, as `on` says.
- A newer commit on the same branch stops the older run.
- A tree that was already tested needs no run at all.
- With the App installed and the machine signed in, a push is enough. The run it starts posts one check per job on the PR, such as `shipcheck/test`, in progress at once and then green or red, and a `shipcheck` summary of the run.
- When a PR changes on GitHub, such as a rebase with the Update branch button, the daemon reruns its checks ([how it works](@/docs/how-it-works.md#when-a-pr-changes-on-github)).

To run the checks in the foreground and wait for the verdict, for example as an agent's test step before it pushes:

```sh
shipcheck run
```

When a run fails, give the agent the logs:

```sh
shipcheck logs
```
