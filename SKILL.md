---
name: shipcheck
description: Run a repository's GitHub Actions CI locally with shipcheck and read its result, so a PR gets its required `shipcheck` check without waiting for hosted CI. Use when a repository has a `shipcheck.toml`, before pushing or opening a PR, when a `shipcheck` check failed, or when asked to set shipcheck up.
---

# shipcheck

shipcheck runs the workflows listed in `shipcheck.toml` on this machine, each job in its own container, for one commit. It keeps the result for that commit's git tree. The GitHub check named `shipcheck` comes from that result, so a passing local run is what lets the PR merge.

Docs: https://shipcheck.run/llms.txt

## Before a push

On a new machine or repository, run `shipcheck doctor` first. It prints what is missing and the command that fixes it. Follow it until it exits 0.

1. Commit your work. shipcheck tests commits, never the working copy, so uncommitted changes do not count.
2. Run `shipcheck run`. It waits for the verdict and prints one line per workflow and one per job.
3. Read the exit code:

| Code | Meaning | Do |
|---|---|---|
| 0 | Every workflow passed, or nothing under `paths` changed | Push |
| 1 | A workflow failed | Run `shipcheck logs`, fix the code, commit, run again |
| 2 | shipcheck could not run | Read the message. Usually `shipcheck.toml` is missing on `main` or act or Docker is not running |

A tree that already passed answers at once, so running it again after a rebase that changed nothing is cheap.

If the repository has hooks installed (`shipcheck install`), a push starts the run in the background on its own and never blocks git. `shipcheck status` prints the result for `HEAD`.

## When it fails

- `shipcheck logs` prints the full log of every job. Find the first failed step.
- Fix the cause in the code or the tests. A job state such as `failure, allowed by continue-on-error` does not fail the workflow.
- `no image for its runs-on label on this host` means a macOS or Windows job, which runs only on GitHub, or a Linux label such as `ubuntu-24.04` with no image mapped in `[act] platforms`. Tell the owner, it is a `shipcheck.toml` question.
- A job that needs the network fails when `shipcheck.toml` has no `network = "internet"`. Jobs get no network by default. Tell the owner, do not work around it.
- A refused workflow names the setting it refused, such as a container option or volume. The setting comes from the owner's workflow on `main`, so tell the owner.

## Never

- Never edit `.github/workflows/` or `shipcheck.toml` to make a check pass. shipcheck reads both from `main`, so the edit does not change what runs, and the check names it under "changes CI" for the reviewer.
- Never weaken, skip or delete tests to get a pass. The check reports it, and a reviewer reads it.
- Never write or copy files under `SHIPCHECK_HOME` by hand. Only runs write results.
- Never run `shipcheck clean` while a run is going. It refuses anyway.

## Set it up

For the owner of a repository:

```sh
shipcheck init      # writes shipcheck.toml with every workflow listed, installs the hooks
```

Then remove deploys and anything that must not run on a laptop from `workflows`, commit `shipcheck.toml`, and merge it to `main`. For each other clone: `shipcheck install`.

To post the `shipcheck` check from this machine, the App must be installed on the repository and the machine signed in once with `shipcheck login`. It prints a code a person enters at github.com/login/device, so ask the human to run it. Then `shipcheck post` posts the result of `HEAD`. `shipcheck doctor` says whether posting works. `shipcheck logout` signs the machine out.

Settings: https://shipcheck.run/docs/config.md
