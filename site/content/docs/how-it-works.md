+++
title = "How it works"
description = "What shipcheck runs, where, and when it trusts a stored result."
weight = 60
+++

## A tool reports the result, never the agent

The agent can start a run. Only shipcheck turns a run into a check, from facts it reads itself: the exit codes of the processes it started, the git tree of the commit, and the config on `main`. An agent that says the tests passed changes nothing.

## What runs

`runner` in `shipcheck.toml` picks one of two modes. `shipcheck init` writes host mode when it finds checks in the workflows or a project file it knows, act mode when a job runs a Linux matrix and Docker is running, and act mode when it finds nothing ([which jobs run where](#which-jobs-run-where)).

- Host mode, `runner = "host"`: the commands the owner lists under `[host] checks`, such as `cargo test` or `npm test`, run natively on this machine in an OS sandbox, each in its own clone. A pass means the code passed on this machine's toolchain, operating system and system libraries ([modes](@/docs/config.md#modes)).
- act mode, `runner = "act"`: the GitHub workflows run in Linux containers with act. This is the mode closest to GitHub's CI: a pass means the workflows as written passed in a container like GitHub's runner. It needs Docker.

shipcheck reads `shipcheck.toml` and `.github/workflows/` from `origin/main`. It never reads them from the branch or from your working copy, so a PR that edits a workflow to `exit 0` or empties `workflows` changes nothing until it merges.

When `paths` is set and a change touches none of those files since it left `main`, nothing runs and every job check is skipped with that reason. shipcheck works this out again on each run, because it depends on where `main` is.

In act mode, before a run, shipcheck edits its own copy of each workflow it runs. The repository's files stay as they are.

- It adds a `workflow_dispatch` trigger, so act can run a workflow that on GitHub runs only on pushes to `main`. A job guarded by `if: github.ref == 'refs/heads/main'`, such as a deploy, is skipped, because the ref is the branch.
- It adds a first and a last step to every job. They print markers with a code that is new for each run, which tell shipcheck that each job started and how it ended.
- It splits each matrix into one job per combination. act does not always pick the same platform for matrix jobs, and separate jobs run the same way every time.

A workflow passes when every job that started reached its end with status `success`, and no job was skipped for an unsupported platform. As on GitHub, a failed job with `continue-on-error: true` does not fail the workflow.

## Which jobs run where

`shipcheck init` sorts each job of a workflow GitHub runs for feature branches:

| Job | For a PR it runs |
|---|---|
| No matrix, on Linux | Here, in host mode |
| A matrix of Linux entries, Docker running | Here, in act. The whole repository uses act mode |
| A matrix of Linux entries, no Docker | On GitHub Actions |
| A matrix with macOS or Windows entries | On GitHub Actions |
| On macOS | Here in host mode on a Mac, on GitHub Actions elsewhere |
| On Windows | On GitHub Actions |
| With an `if:` on the PR or the default branch | On GitHub Actions, its workflow as it is |

A matrix of one combination is a plain job. A matrix exists to test several toolchains or systems, and host mode tests only the one on this machine, so a matrix job runs in act or stays on GitHub. That holds for a matrix such as express's, whose `run` steps never name the matrix, since its setup actions do. act runs only Linux containers, so `init` picks act mode only when every job runs on Linux.

shipcheck does not support Windows yet, so Windows jobs stay on GitHub Actions ([limits](@/docs/limits.md#windows)).

`init` prints each job that stays on GitHub and why. A workflow with such a job is kept whole: its triggers stay, and each job shipcheck runs gets `if: github.ref == 'refs/heads/main'`, and-ed with an `if:` it already has. On a PR, GitHub then runs only the jobs that stay, and on `main` it runs them all. This works for `pull_request` and for `push` on every branch. A job that a job on GitHub needs, directly or through others, stays on GitHub too, since GitHub skips a job whose needs it skipped. If you decline the edit, the workflow keeps running fully on GitHub for PRs, beside shipcheck's checks.

The ruleset `init` offers requires `shipcheck/<job>` from the shipcheck App and, from GitHub Actions, the checks of the jobs that stay on GitHub. When their names depend on a matrix `include` or an expression, `init` says so and you add them once a PR shows them. Jobs that stay on GitHub still use Actions minutes.

To move a repository to act anyway, set `runner = "act"` and list the workflows in `workflows` in `shipcheck.toml` ([modes](@/docs/config.md#modes)). To give a job back to GitHub, remove its check from `[host.checks]` and its `if:` from the workflow.

## When it runs

Runs start from the hooks, as `on` in `shipcheck.toml` says, or from `shipcheck run`.

`shipcheck run` tests the working copy, so the test run an agent does before it commits becomes the check. shipcheck builds a tree from the working copy in a copy of the index: tracked files with their edits and untracked files that are not gitignored. Your branch, index and stash stay as they are.

`--staged` builds it from the index instead. When you commit that same tree, with `shipcheck commit` or with git, the commit hook and `shipcheck post` find the result by tree and nothing runs again. A commit with any other tree, such as one with a file left out, runs again.

- A run from a hook goes to the background. The commit or push carries on at once, and the run's output goes to `logs/background.log` in shipcheck's home.
- There is one background run per repository and branch. A newer commit stops the older run, its containers included, before its own starts, like `cancel-in-progress` in a workflow. A request for the commit already running leaves that run alone, so a push right after a commit does not restart it.
- On a machine signed in with `shipcheck login`, a push's run also posts the checks. Once GitHub has the commit, every check shows in progress, and when the run ends it turns green or red. A tree that already passed posts at once. A newer push stops the older run, which posts nothing. A machine that is not signed in writes one line to `logs/background.log` and posts nothing.
- `shipcheck run` runs in the foreground and waits, for when you or an agent want the verdict before going on.

## When a PR changes on GitHub

A rebase or an update on GitHub, such as the Update branch button, makes a new head commit with no local push, so no hook runs. The daemon covers that case. `shipcheck init` and `shipcheck install` start it as a background service.

- Every 10 seconds by default, it reads the open PRs into `main` of each repository it was added to.
- When a head has no completed `shipcheck` check and its tree has a stored result, it posts that result at once. A rebase that changes no file has the same tree, so it needs no run.
- When the tree has no result, it runs the checks and posts them, the same way a push does. A newer head stops the older run, which posts nothing.
- It runs a head only when you, or a login `daemon_authors` in `shipcheck.toml` on `main` lists, both opened the PR and pushed that head. It never runs a fork's PR.
- "Update branch" and a rebase on GitHub are pushes by whoever clicked the button. So when you click it on your own PR, your daemon runs the new head. When a teammate clicks it, your daemon refuses the head unless `daemon_authors` lists them. To check it yourself, pull the branch, then run `shipcheck run --sha <head>` and `shipcheck post <head>`.
- GitHub can take from seconds to many minutes to list a push, so a run can start late. A head whose push is still not listed after 30 minutes is refused.
- A refused head stays refused for 30 days, and so does every later head that contains it, such as a merge "Update branch" made on top of it. A head whose tree already has a result is never refused: the daemon posts that result.
- With several machines running it, the first to mark a head in progress runs it, and the others wait. Two that start within a few seconds of each other can both run it, and both post the same result.

`daemon = false` in `shipcheck.toml` keeps it off a repository, and `shipcheck daemon --uninstall` removes it from the machine.

## Where it runs

In act mode each job gets a new checkout of the tested tree, cloned with copy-on-write from its own warm template, so it starts with the build output of its earlier runs and costs no disk. A working copy is checked out from a commit no ref names, with `HEAD` as parent, as `git stash create` makes. A run never touches your own checkout. Jobs never share a checkout, as on GitHub. A job starts once the jobs in its `needs` pass, and gets their outputs. Gitignored files are not in it.

The checkout sits at a fixed path per repository, slot and job, because builds remember absolute paths. Toolchains and package caches live in Docker volumes per repository, removed after 14 days without a run that uses them. apt's package lists get a volume per slot and job, and `.venv` a new volume each run, off the macOS bind mount. setup-uv steps get `prune-cache: false` in the job's copy, so uv's cache keeps its wheels, and `pnpm/action-setup`'s `run_install` becomes plain `pnpm install` steps after it, since the action prunes the store after an install. Both are fed only by shipcheck's own runs, never by builds on your machine, which would not run in a Linux container.

act runs each job in Docker, never on the machine itself. A job gets its checkout, its cache volumes, no Docker socket, no host token or environment, and by default no network.

In host mode each check gets the same kind of clone, but its template is warmed from your checkout at most once an hour, so gitignored `node_modules` and `target/` are there. It runs in `sandbox-exec` on macOS or bubblewrap on Linux: it writes only its clone, a new home and the repository's tool caches, reads the system and your toolchains, and has the network ([config](@/docs/config.md#modes)).

## When a stored result counts

shipcheck stores each result under a key made of:

- the git tree that ran
- `shipcheck.toml` on `main`
- each workflow as shipcheck runs it, or in host mode each check's command and the output of `versions`
- the act and shipcheck versions
- the pinned images

Two commits with the same tree share a result, so a rebase or a squash that changes nothing needs no new run. A change to the config or the workflows on `main` changes the key, so old results stop counting.

Only a pass is reused. A failure runs again next time, so a flaky failure cannot stick.

Two runs of the same tree at once share one run. The second waits, then reads the result.

## The checks on the PR

shipcheck posts one check per job, `shipcheck/test` for job `test`, as GitHub Actions does, and branch protection requires them ([require the checks](@/docs/branch-protection.md)). Each is queued until a machine runs the commit, in progress while it runs, then green or red. A `paths` skip marks every job skipped. A `shipcheck` check beside them sums up the run, with each failed workflow's log tail.

Each job check shows how long the job took and, as `logs` in `shipcheck.toml` says, the end of its log ([config](@/docs/config.md)):

- The last 100 lines. A longer log starts with `[...]`, and a host check's log keeps its command line above it, so you still see what ran.
- A failed job lists its failed tests first, then each test's own output, such as its panic, assertion or traceback, even when it is far above the last lines. This works for cargo test, nextest, pytest, jest, vitest and go test.
- When the failures do not fit in GitHub's 65,535 characters, the check keeps the first ones whole and says how many it left out. `shipcheck logs` has the rest.
- Values from the secrets file are replaced with `***`. A log can still hold paths, names or a secret printed in another form.
- A fork's PR never gets a log.

## The report comment

Write a report to `$GITHUB_STEP_SUMMARY` and it appears in one shipcheck comment on the PR. This replaces a step that posts a coverage report with `gh pr comment`, which cannot run under shipcheck: a job has no GitHub token and, by default, no network.

- Each job check gets its own section, failed jobs first. Every step that writes to the file adds to its job's section in step order, as on GitHub.
- The App writes the comment once and then edits it. A later post replaces the sections of the jobs it ran and keeps the others, so a rerun of one job changes only that job's section. A job that ran again and wrote nothing loses its section.
- If someone edited the comment and its hidden markers no longer match, the next post writes the whole comment again from its own result.
- A run where no job wrote a summary posts nothing and deletes nothing.
- Each job keeps at most 20 KB of its summary. The comment stays under GitHub's 65,536 characters, and a section that does not fit says it was left out.
- Values from the secrets file are replaced with `***`.
- A fork's PR never gets the comment, since the fork's own code wrote the Markdown.

It works in both modes. In act mode, act gives each step the file. In host mode, each check gets `GITHUB_STEP_SUMMARY` set to a file in its sandbox. The App needs Pull requests: write for the comment. Until it has it, the checks still post, and `shipcheck post` prints one line saying there is no comment.
