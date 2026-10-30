---
name: shipcheck
description: Run a repository's CI checks locally with shipcheck and read the result, so a PR gets its required `shipcheck/<job>` checks without waiting for hosted CI. Use when a repository has a `shipcheck.toml`, before pushing or opening a PR, when a `shipcheck` check failed, or when asked to set shipcheck up.
---

# shipcheck

shipcheck runs the checks that `shipcheck.toml` names on this machine, for your working copy or a commit, and keeps the result for that git tree. The PR's required checks, one per job such as `shipcheck/test`, come from that result, so a passing local run is what lets the PR merge.

## Install

- macOS: `brew install gabrielkoerich/tap/shipcheck`. It also installs act, git and the Docker client.
- Linux, or without Homebrew: `curl -fsSL https://shipcheck.run/install.sh | sh`. It installs to `~/.local/bin`, and `SHIPCHECK_INSTALL_DIR` changes the folder.
- With cargo: `cargo binstall shipcheck`.

All three install the release binary, which on macOS is signed and notarized. Host mode needs no Docker. act mode needs act 0.2.86 or later and a running Docker engine: Docker Desktop 4.44.3 or later, OrbStack, or colima as the Docker engine, with runc 1.2.8, 1.3.3 or later. `engine = "colima"` needs colima and lima on top of that.

Then run `shipcheck doctor`. On a new machine or repository, it prints what is missing and the command that fixes it. Follow it until it exits 0.

## Modes

There are two modes. With `runner = "host"`, which `shipcheck init` writes when it finds checks, the checks are the owner's commands under `[host.checks]`, each in its own clone inside an OS sandbox, with no Docker. With `runner = "act"`, the default, the checks are the jobs of the workflows listed in `workflows`, each in its own container on this machine's Docker engine. With `runner = "host"` and no `[jobs]` table, `workflows` must be empty, or the run exits 2. A job then runs under act through a `[jobs]` table with `runner = "act"`, below. A repository without `.github/workflows` uses host mode.

`runner` sets the mode of every job. A `[jobs."<file>/<job>"]` table changes it for one job, in the same run and under the same `shipcheck` check:

- `runner = "act"` runs that job, and the jobs it needs, under act beside the host checks.
- `runner = "host"` takes that job out of a listed workflow, and the `[host.checks]` entry of the same id runs in its place.
- `engine = "colima"` runs that act job on a Colima VM of its own, with its own Docker and kernel, for a job that runs Docker or needs a Linux kernel feature. Each job's VM is a clone of a base VM shipcheck set up once, and shipcheck deletes it after the result. It needs colima, lima and act on the Mac, and a running Docker engine there too, since shipcheck checks Docker before any act job, colima ones included. The job's steps run on the VM's own Docker. This machine fetches the job's actions. The VM reaches this machine's loopback and the internet, so use it only for code you trust. shipcheck's own repository sets `engine = "colima"` for its `ci.yml/test` job, so every shipcheck PR's `shipcheck/test` check comes from a colima VM. Its end-to-end tests run against real VMs by hand on a Mac with colima, and check that each job gets its own VM and leaves none behind, that no port of the VM reaches the Mac's loopback, that a killed run's VM is still deleted, that the base VM and other colima profiles stay, that a failing or timed-out job leaves no VM, and that a job its `if:` skips boots no VM. They passed on 2026-10-30. This engine is apart from colima as the Docker engine behind act jobs, which runs each job in a container like Docker Desktop.

A host check and an act job may not share a name, since both would post as `shipcheck/<name>`. The run exits 2 and names the two: rename the host check, for example `test` to `test-macos`. `--job ci.yml/e2e` picks the job of one workflow.

In this file, `main` is the remote's default branch.

Docs: https://shipcheck.run/llms.txt

## The rule

Do not run the project's test commands yourself. Run `shipcheck run`, and when it passes, `shipcheck commit -m <message>`. `shipcheck commit` and `shipcheck post` refuse on the default branch, since PRs carry the checks. `shipcheck run` works anywhere. Work in small steps and commit each one. Every commit is then a tested tree and gets its check at once, with no second run.

## Each step

1. Run `shipcheck run` before you commit. It tests the working copy: tracked edits and every untracked file that is not gitignored. When it passes, `shipcheck commit -m <message>` commits exactly the tree that ran.
2. To commit only part of your work, stage it and run `shipcheck run --staged`, then `shipcheck commit -m <message>`, which commits the index as it ran. `shipcheck status` says whether the working copy and the index already have a result.
3. Read the exit code:

| Code | Meaning | Do |
|---|---|---|
| 0 | Every check passed, or nothing under `paths` changed | `shipcheck commit -m <message>`, push when the work is done |
| 1 | A check failed | Read the failed check, step, tests and output it printed, fix the code, run again (below) |
| 2 | shipcheck could not run | Read the message. Often `shipcheck.toml` on `main` is missing or invalid, the OS sandbox is missing in host mode, or Docker is not running or failed to start a job in act mode. The code did not run, so do not change it: fix the machine, `shipcheck doctor` helps, then run again |

With no `shipcheck.toml` on `main` there is nothing to run. `shipcheck run` exits 2 and says so, and the hooks and the daemon start nothing. The fix is `shipcheck init`, then merge `shipcheck.toml` to `main`, which the owner does.

A tree that already passed answers at once, so a run after a rebase that changed nothing is cheap.

`shipcheck run` lists the untracked files it includes. Gitignore or delete the ones that are not part of the change, or the commit's tree differs from the one that ran. `shipcheck commit` refuses when a file changed since the run: run `shipcheck run` again.

`shipcheck install` installs git hooks. Each push of a tree with no result to a branch other than the default starts a run in the background, and so does each commit when `on` in `shipcheck.toml` includes `commit`. Git never waits for it. On a signed-in machine a push is enough: its run posts the checks on the PR, so after `shipcheck commit` you only `git push`. When you rebase or update your PR on GitHub, the daemon that `install` starts runs the new head. `shipcheck run --sha HEAD` tests the last commit and ignores the working copy.

## When it fails

- For each failed job or host check, the output names the step and its exit code in act mode, the failing tests, the last 60 lines of the step or check, and the path of the full log. Start there and fix the cause in the code or the tests. `shipcheck logs` prints every log, and `shipcheck run --json` prints the same report as one object.
- A failed `shipcheck/<job>` check on the PR lists the failed tests first, then each test's own output, then the last 100 lines of the job's log. `shipcheck logs <sha>` prints the whole log on the machine that ran it.
- Fix, then run `shipcheck run --job <id>` as often as you need. In act mode it runs only that job and the jobs it `needs`, and in host mode only that check, so you see the result in seconds. `--workflow <file>` runs every job of one workflow, and both flags repeat. A partial run is never the check: it stores no result, and `shipcheck commit` refuses after it, even with `--allow-failed`. Its output ends with `partial run: not the check`. When the job passes, run one full `shipcheck run`, then `shipcheck commit -m <message>`. `shipcheck logs` prints the last partial run's logs after the full ones.
- In host mode, `--workflow` is an error, since host mode runs checks, not workflows.
- A job state such as `failure, allowed by continue-on-error` does not fail the workflow.
- In act mode, `failure, act skipped it by its if:` means act skipped a job whose `if:` may hold on a PR. act runs a PR's checks as `workflow_dispatch`, so only GitHub can tell. A skip stands only when the `if:` is false on every PR: `false`, a push event, one branch or a tag. A job that needs a skipped job and whose `if:` uses `always()`, `failure()` or `cancelled()` fails the same way. A step act skips by an `if:` that reads the event, the ref, the actor or `vars`, directly or through an `env.` value, fails its job the same way, with `act skipped the step '<name>' by its if:` from the `shipcheck if` step before it. Steps false on every PR, and `failure()` or `cancelled()` steps, are left alone. The fix is in the workflow on `main`, so tell the owner: keep the job on GitHub, or make the `if:` false on every PR.
- In host mode, `Operation not permitted` or `Permission denied` on a path outside the clone, in your home, or on a socket means the test reached outside the sandbox. Fix the test to use its working directory, `$HOME` or `$TMPDIR`. If a toolchain in your home cannot be read, tell the owner. Never ask for `sandbox = false` to get past a denial, and never run `shipcheck trust`: together they run the check with the user's full access, so both are the owner's choice for a check that needs a system service, such as the iOS simulator.
- In act mode, `no image for its runs-on label on this host` means a macOS or Windows job, which runs only on GitHub, or a Linux label such as `ubuntu-24.04` with no image in `[act] platforms`. Tell the owner, since it is a `shipcheck.toml` question.
- In act mode, jobs get no network by default, so a job that needs it fails unless `shipcheck.toml` sets `network = "internet"`. Tell the owner, do not work around it.
- A refused workflow names the setting it refused, such as a container option or a volume. The setting comes from the owner's workflow on `main`, so tell the owner.
- `checks not posted: the shipcheck App is not installed on <repo>, fix: add <repo> to the shipcheck App: https://github.com/settings/installations (shipcheck-run, Configure)` means the run finished on this machine but the PR got no checks. `shipcheck status`, the end of `shipcheck run` and the next push print it, `shipcheck logs` ends with `posting: not posted`, `shipcheck history` shows `not posted (no App)`, and the `app` line of `shipcheck doctor` fails. Only the owner can add the repository to the App, so tell them. The line goes away after the next post that succeeds.

## Never

- Never edit `.github/workflows/` or `shipcheck.toml` to make a check pass. shipcheck reads both from `main`, so the edit does not change what runs, and the check lists it under "changes CI" for the reviewer. The check also lists changes to what a command runs, such as `package.json` scripts, a `Makefile` or a `justfile`.
- Never weaken, skip or delete tests to get a pass. The check reports it, and a reviewer reads it.
- Never write or copy files under `SHIPCHECK_HOME` by hand, and never set `SHIPCHECK_HOME`. Only runs write results.
- Never run `shipcheck clean` while a run is going. It refuses anyway.

## Set up a repository: `shipcheck init`

The repository owner runs this once per repository, in a clone on `main`. An agent runs it with `--yes`:

```sh
shipcheck init --yes
```

1. It writes `shipcheck.toml` with a check for each workflow job it can run here, and prints what it left out and why. A job no host check runs as GitHub does, such as one with a heredoc step, an action no command stands for, a `container:` or a JDK this machine lacks, goes to act as a `[jobs."<file>/<job>"]` entry with `runner = "act"` when Docker runs here, and `network = "internet"` comes with it, since those jobs download what their steps set up. A job stays on GitHub with the reason when act cannot run it as GitHub does either: macOS or Windows, a runner label act has no image for, an `if:`, a value, a script or `HEAD` that reads the PR, a PR-aware action, an action that reports to GitHub, or Docker or a kernel change inside the job. colima can run that last kind. init writes `engine = "colima"` only when the person moves the job to a colima VM on the Checks tab, never by default, since the VM reaches this machine's loopback. In host mode init then runs each check once in the sandbox, as `shipcheck run` would. A check that fails is tried again without forced color, then with color off. A job whose check still fails stays on GitHub, and init prints the end of its log and the fix. When every job has to stay on GitHub, it writes no file and stops, and `shipcheck run` then has nothing to run.
2. On a terminal its steps are tabs, Checks, Install, Workflows, GitHub and Review, which the person moves between and goes back to: where each job runs (a host check, act, a colima VM the person opts into, or GitHub), the [shipcheck App](https://github.com/apps/shipcheck-run) link, hosted Actions only on `main`, and a ruleset that requires each `shipcheck/<job>` check and PRs into the default branch. For a job on GitHub whose check names init cannot tell, such as a job whose name or matrix comes from an expression, the ruleset keeps the checks the branch requires now. When `gh` cannot read the branch's rules, init makes no ruleset and leaves them as they are. It changes nothing until the person applies on the Review tab. `--yes` applies the recommended choices with no questions, but opens no browser and creates no ruleset.
3. It writes the agent instructions into `AGENTS.md` and the other files agents read, then does what `shipcheck install` does.

Without a terminal and without `--yes` it tries each check and prints the plan and the review that `--yes` would apply, then writes nothing: no `shipcheck.toml`, no `AGENTS.md`, no hooks, no daemon and no sign-in. Run `shipcheck init --yes` to apply it. Then read the checks it wrote, commit `shipcheck.toml`, `AGENTS.md` and the workflow changes, and merge them to `main`. The owner installs the App on the repository once.

## Set up a machine: `shipcheck install`

Each machine that runs agents runs this once per clone:

```sh
shipcheck install
```

It installs the git hooks, starts the daemon, and on a terminal signs the machine in when it cannot post yet, with a code a person enters at github.com/login/device. When it says not signed in, ask the human to run `shipcheck login`.

A machine with no person at it, such as a VM, gets a token instead. A person runs `shipcheck token --name <machine>` on their own machine and sets the output as `SHIPCHECK_TOKEN` on the agent's machine. `shipcheck logout` there, or `shipcheck logout --all` anywhere, revokes it. `logout` revokes only shipcheck machine tokens, `--all` every one of the GitHub user's.

Checks post only through the [shipcheck App](https://github.com/apps/shipcheck-run) and the hosted service. Once signed in, each push posts its checks, and `shipcheck post` posts the stored result of `HEAD`, or of the commit you name. It refuses the tip of the default branch. `shipcheck doctor` says whether posting works. On macOS a keychain error after an upgrade means the new binary needs access: ask the human to run `shipcheck doctor` at the Mac and choose Always Allow.

Settings: https://shipcheck.run/docs/config.md
