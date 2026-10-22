---
name: shipcheck
description: Run a repository's GitHub Actions CI locally with shipcheck and read its result, so a PR gets its required `shipcheck` check without waiting for hosted CI. Use when a repository has a `shipcheck.toml`, before pushing or opening a PR, when a `shipcheck` check failed, or when asked to set shipcheck up.
---

# shipcheck

shipcheck runs the checks `shipcheck.toml` names on this machine, for your working copy or a commit. With `runner = "host"` these are the owner's commands under `[host] checks`, each in its own clone inside an OS sandbox. With `runner = "act"` they are the listed GitHub workflows, each job in its own container. It keeps the result for that git tree. The GitHub check named `shipcheck` comes from that result, so a passing local run is what lets the PR merge.

Docs: https://shipcheck.run/llms.txt

## The rule

Do not run the project's test commands yourself. Run `shipcheck run`, and when it passes, `shipcheck commit -m <message>`. Work in small steps and commit each one. Every commit is then a tested tree and gets its check at once, with no second run.

## Each step

On a new machine or repository, run `shipcheck doctor` first. It prints what is missing and the command that fixes it. Follow it until it exits 0.

1. Run `shipcheck run` before you commit. It tests the working copy: tracked edits and every untracked file that is not gitignored. When it passes, `shipcheck commit -m <message>` commits exactly the tree that ran.
2. If you stage only part of your work and commit the index, run `shipcheck run --staged` instead, then `shipcheck commit -m <message>`, which commits the index as it ran. `shipcheck status` says whether the working copy and the index already have a result.
3. Read the exit code:

| Code | Meaning | Do |
|---|---|---|
| 0 | Every workflow passed, or nothing under `paths` changed | `shipcheck commit -m <message>`, push when the work is done |
| 1 | A workflow failed | Run `shipcheck logs`, fix the code, run again |
| 2 | shipcheck could not run | Read the message. Usually `shipcheck.toml` is missing on `main` or act or Docker is not running |

A tree that already passed answers at once, so running it again after a rebase that changed nothing is cheap.

`shipcheck run` lists the untracked files it includes. Gitignore or delete the ones that are not part of the change, or the commit's tree differs from the one that ran. `shipcheck commit` refuses when a file changed since the run: run `shipcheck run` again.

With `shipcheck install`, your agent's own hooks deny the test commands and `git commit` with the command to use instead, and stop you from ending a turn with uncommitted or untested work, once. A merge, revert, cherry-pick or rebase in progress may still finish with `git commit`. Never work around a deny with `sh -c` or a script.

If the repository has git hooks installed (`shipcheck install`), a commit or push of a tree that has no result starts a run in the background and never blocks git. `shipcheck run --sha HEAD` tests the last commit and ignores the working copy.

## When it fails

- `shipcheck logs` prints the full log of every job. Find the first failed step.
- Fix the cause in the code or the tests. A job state such as `failure, allowed by continue-on-error` does not fail the workflow.
- `no image for its runs-on label on this host` means a macOS or Windows job, which runs only on GitHub, or a Linux label such as `ubuntu-24.04` with no image mapped in `[act] platforms`. Tell the owner, it is a `shipcheck.toml` question.
- A job that needs the network fails when `shipcheck.toml` has no `network = "internet"`. Jobs get no network by default. Tell the owner, do not work around it.
- With `runner = "host"`, each check runs in an OS sandbox. `Operation not permitted` or `Permission denied` on a path outside the clone, in your home, or on a socket means the test reached outside it. Fix the test to use its working directory, `$HOME` or `$TMPDIR`. If a toolchain in your home cannot be read, tell the owner.
- A refused workflow names the setting it refused, such as a container option or volume. The setting comes from the owner's workflow on `main`, so tell the owner.

## Never

- Never edit `.github/workflows/` or `shipcheck.toml` to make a check pass. shipcheck reads both from `main`, so the edit does not change what runs, and the check names it under "changes CI" for the reviewer. The same goes for what a command runs, such as `package.json` scripts, a `Makefile` or a `justfile`: the check names those changes too.
- Never weaken, skip or delete tests to get a pass. The check reports it, and a reviewer reads it.
- Never write or copy files under `SHIPCHECK_HOME` by hand, and never set `SHIPCHECK_HOME`. Only runs write results.
- Never run `shipcheck clean` while a run is going. It refuses anyway.

## Set it up

For the owner of a repository:

```sh
shipcheck init      # writes shipcheck.toml and the AGENTS.md block, installs the hooks
```

Then remove deploys and anything that must not run on a laptop from `workflows`, commit `shipcheck.toml`, and merge it to `main`. For each other clone: `shipcheck install`.

To post the `shipcheck` check from this machine, the App must be installed on the repository and the machine signed in once with `shipcheck login`. It prints a code a person enters at github.com/login/device, so ask the human to run it. Then `shipcheck post` posts the result of `HEAD`. `shipcheck doctor` says whether posting works. `shipcheck logout` signs the machine out.

Settings: https://shipcheck.run/docs/config.md
