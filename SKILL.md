---
name: shipcheck
description: Run a repository's CI checks locally with shipcheck and read the result, so a PR gets its required `shipcheck/<job>` checks without waiting for hosted CI. Use when a repository has a `shipcheck.toml`, before pushing or opening a PR, when a `shipcheck` check failed, or when asked to set shipcheck up.
---

# shipcheck

shipcheck runs the checks that `shipcheck.toml` names on this machine, for your working copy or a commit, and keeps the result for that git tree. The PR's required checks, one per job such as `shipcheck/test`, come from that result, so a passing local run is what lets the PR merge.

There are two modes. With `runner = "host"`, which `shipcheck init` writes when it finds checks, the checks are the owner's commands under `[host.checks]`, each in its own clone inside an OS sandbox. With `runner = "act"`, the checks are the listed GitHub workflows, and each job runs in its own container. `runner` is the default for every job: a `[jobs."ci.yml/e2e"]` table with `runner = "act"` runs that one job, and the jobs it needs, with act beside the host checks, in the same run and under the same `shipcheck` check. `shipcheck run --job ci.yml/e2e` runs one job of one workflow.

Docs: https://shipcheck.run/llms.txt

## The rule

Do not run the project's test commands yourself. Work on a branch: `git switch -c <branch>`. Run `shipcheck run`, and when it passes, `shipcheck commit -m <message>`. `shipcheck commit` and `shipcheck post` refuse on the default branch, since PRs carry the checks. `shipcheck run` works anywhere. Work in small steps and commit each one. Every commit is then a tested tree and gets its check at once, with no second run.

## Each step

On a new machine or repository, run `shipcheck doctor` first. It prints what is missing and the command that fixes it. Follow it until it exits 0.

1. Run `shipcheck run` before you commit. It tests the working copy: tracked edits and every untracked file that is not gitignored. When it passes, `shipcheck commit -m <message>` commits exactly the tree that ran.
2. To commit only part of your work, stage it and run `shipcheck run --staged`, then `shipcheck commit -m <message>`, which commits the index as it ran. `shipcheck status` says whether the working copy and the index already have a result.
3. Read the exit code:

| Code | Meaning | Do |
|---|---|---|
| 0 | Every check passed, or nothing under `paths` changed | `shipcheck commit -m <message>`, push when the work is done |
| 1 | A check failed | Read the failed check, step, tests and output it printed, fix the code, run again (below) |
| 2 | shipcheck could not run | Read the message. Often `shipcheck.toml` on `main` is invalid, the OS sandbox is missing in host mode, or Docker is not running or failed to start a job in act mode. The code did not run, so do not change it: fix the machine, `shipcheck doctor` helps, then run again |

A tree that already passed answers at once, so a run after a rebase that changed nothing is cheap.

`shipcheck run` lists the untracked files it includes. Gitignore or delete the ones that are not part of the change, or the commit's tree differs from the one that ran. `shipcheck commit` refuses when a file changed since the run: run `shipcheck run` again.

`shipcheck install` installs git hooks. Each push of a tree with no result starts a run in the background, and so does each commit when `on` in `shipcheck.toml` includes `commit`. Git never waits for it. On a signed-in machine a push is enough: its run posts the checks on the PR, so after `shipcheck commit` you only `git push`. When you rebase or update your PR on GitHub, the daemon that `install` starts runs the new head. `shipcheck run --sha HEAD` tests the last commit and ignores the working copy.

## When it fails

- For each failed job or host check, the output names the step and its exit code in act mode, the failing tests, the last 60 lines of the step or check, and the path of the full log. Start there and fix the cause in the code or the tests. `shipcheck logs` prints every log, and `shipcheck run --json` prints the same report as one object.
- A failed `shipcheck/<job>` check on the PR lists the failed tests first, then each test's own output, then the last 100 lines of the job's log. `shipcheck logs <sha>` prints the whole log on the machine that ran it.
- Fix, then run `shipcheck run --job <id>` as often as you need. In act mode it runs only that job and the jobs it `needs`, and in host mode only that check, so you see the result in seconds. `--workflow <file>` runs every job of one workflow, and both flags repeat. A partial run is never the check: it stores no result, and `shipcheck commit` refuses it. Its output ends with `partial run: not the check`. When the job passes, run one full `shipcheck run`, then `shipcheck commit -m <message>`. `shipcheck logs` prints the last partial run's logs after the full ones.
- In host mode, `--workflow` is an error, since host mode runs checks, not workflows.
- A job state such as `failure, allowed by continue-on-error` does not fail the workflow.
- In host mode, `Operation not permitted` or `Permission denied` on a path outside the clone, in your home, or on a socket means the test reached outside the sandbox. Fix the test to use its working directory, `$HOME` or `$TMPDIR`. If a toolchain in your home cannot be read, tell the owner.
- In act mode, `no image for its runs-on label on this host` means a macOS or Windows job, which runs only on GitHub, or a Linux label such as `ubuntu-24.04` with no image in `[act] platforms`. Tell the owner, since it is a `shipcheck.toml` question.
- In act mode, jobs get no network by default, so a job that needs it fails unless `shipcheck.toml` sets `network = "internet"`. Tell the owner, do not work around it.
- A refused workflow names the setting it refused, such as a container option or a volume. The setting comes from the owner's workflow on `main`, so tell the owner.

## Never

- Never edit `.github/workflows/` or `shipcheck.toml` to make a check pass. shipcheck reads both from `main`, so the edit does not change what runs, and the check lists it under "changes CI" for the reviewer. The check also lists changes to what a command runs, such as `package.json` scripts, a `Makefile` or a `justfile`.
- Never weaken, skip or delete tests to get a pass. The check reports it, and a reviewer reads it.
- Never write or copy files under `SHIPCHECK_HOME` by hand, and never set `SHIPCHECK_HOME`. Only runs write results.
- Never run `shipcheck clean` while a run is going. It refuses anyway.

## Set up a repository: `shipcheck init`

The repository owner runs this once per repository, in a clone on `main`:

```sh
shipcheck init
```

1. It writes `shipcheck.toml` with a check for each workflow job it can run here, and prints what it left out and why. In host mode it first runs each check once in the sandbox, as `shipcheck run` would. A check that fails is tried again without forced color, then with color off. A job whose check still fails stays on GitHub, and init prints the end of its log and the fix. When every job has to stay on GitHub, it writes no file and stops.
2. On a terminal it walks the further steps: the [shipcheck App](https://github.com/apps/shipcheck-run) link, hosted Actions only on `main`, and a ruleset that requires each `shipcheck/<job>` check and PRs into the default branch. It changes nothing until the person applies the review. `--yes` applies the recommended choices with no questions, but opens no browser and creates no ruleset.
3. It writes the agent instructions into `AGENTS.md` and the other files agents read, then does what `shipcheck install` does.

Without a terminal it writes every check that passed and the instructions, installs the hooks and starts the daemon, and skips the App, workflow and ruleset steps. Read the checks it wrote, commit `shipcheck.toml` and `AGENTS.md`, and merge them to `main`. The owner installs the App on the repository once.

## Set up a machine: `shipcheck install`

Each machine that runs agents runs this once per clone:

```sh
shipcheck install
```

It installs the git hooks, starts the daemon, and on a terminal signs the machine in when it cannot post yet, with a code a person enters at github.com/login/device. When it says not signed in, ask the human to run `shipcheck login`.

A machine with no person at it, such as a VM, gets a token instead. A person runs `shipcheck token --name <machine>` on their own machine and sets the output as `SHIPCHECK_TOKEN` on the agent's machine. `shipcheck logout` there, or `shipcheck logout --all` anywhere, revokes it.

Once signed in, each push posts its checks, and `shipcheck post` posts the stored result of `HEAD`, or of the commit you name. It refuses the tip of the default branch. `shipcheck doctor` says whether posting works. On macOS a keychain error after an upgrade means the new binary needs access: ask the human to run `shipcheck doctor` at the Mac and choose Always Allow.

Settings: https://shipcheck.run/docs/config.md
