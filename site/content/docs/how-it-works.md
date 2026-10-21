+++
title = "How it works"
description = "What shipcheck runs, where, and when it trusts a stored result."
weight = 60
+++

## A tool reports the result, never the agent

The agent can start a run. Only shipcheck turns a run into a check, from facts it reads itself: the exit codes of the processes it started, the git tree of the commit, and the config on `main`. An agent that says the tests passed changes nothing.

## What runs

shipcheck reads `shipcheck.toml` and `.github/workflows/` from `origin/main`. It never reads them from the branch or from your working copy, so a PR that edits a workflow to `exit 0` or empties `workflows` changes nothing until it merges.

When `paths` is set and a change touches none of those files since it left `main`, nothing runs and the check passes with that reason. This is worked out again each time, because it depends on where `main` is.

Before a run, shipcheck edits its own copy of each workflow it runs. The repository's files stay as they are.

- It adds a `workflow_dispatch` trigger, so act can run a workflow that on GitHub runs only on pushes to `main`. A job guarded by `if: github.ref == 'refs/heads/main'`, such as a deploy, is skipped, because the ref is the branch.
- It adds a first and a last step to every job. They print markers with a code that is new for each run, which tell shipcheck that each job started and how it ended.
- It splits each matrix into one job per combination. act picks the platform of matrix jobs inconsistently, and separate jobs run the same way every time.

A workflow passes when every job that started reached its end with status `success`, and no job was skipped for an unsupported platform. As on GitHub, a failed job with `continue-on-error: true` does not fail the workflow.

## When it runs

Runs start from the hooks, as `on` in `shipcheck.toml` says, or from `shipcheck run`.

- A run from a hook goes to the background. The commit or push carries on at once, and the run's output goes to `logs/background.log` in shipcheck's home.
- There is one background run per repository and branch. A newer commit stops the older run, its containers included, before its own starts, like `cancel-in-progress` in a workflow. A request for the commit already running leaves that run alone, so a push right after a commit does not restart it.
- `shipcheck run` runs in the foreground and waits, for when you or an agent want the verdict before going on.

## Where it runs

Each job gets a new checkout of the commit, cloned with copy-on-write from its own warm template, so it starts with the build output of its earlier runs and costs no disk. Jobs never share a checkout, as on GitHub. A job starts once the jobs in its `needs` pass, and gets their outputs. Files you never committed are not in it.

The checkout sits at a fixed path per repository, slot and job, because builds remember absolute paths. Toolchains and package caches live in Docker volumes per repository. Both are fed only by shipcheck's own runs, never by builds on your machine, which would not run in a Linux container.

act runs each job in Docker, never on the machine itself. A job gets its checkout, its cache volumes, no Docker socket, no host token or environment, and by default no network.

## When a stored result counts

shipcheck stores each result under a key made of:

- the git tree of the commit
- `shipcheck.toml` on `main`
- each workflow as shipcheck runs it
- the act and shipcheck versions
- the pinned images

Two commits with the same tree share a result, so a rebase or a squash that changes nothing needs no new run. A change to the config or the workflows on `main` changes the key, so old results stop counting.

Only a pass is reused. A failure runs again next time, so a flaky failure cannot stick.

Two runs of the same tree at once share one run. The second waits, then reads the result.
