+++
title = "What a green check proves"
description = "What a passing shipcheck job check guarantees, and what it does not."
weight = 62
+++

A green `shipcheck/<job>` check on a PR means shipcheck ran that job on the PR's tree, with the config from `main`, and the job passed. The rules below hold for every check.

## What it guarantees

- The result comes only from a run shipcheck started. shipcheck reads the exit codes of the processes it started. Nothing accepts a result, a log or a verdict from an agent or a script, so an agent that says the tests passed changes nothing.
- `shipcheck.toml` and the workflows come from `main`. shipcheck never reads them from the branch or the working copy. A PR that edits a workflow to `exit 0`, or empties `workflows`, is still checked with `main`'s version.
- The job list comes from `main`. A job that `main` has and the result lacks fails, so a PR cannot drop one.
- Each job runs in a new copy-on-write clone of the tested tree, never in the agent's worktree, and never in a checkout another job used.
- A job passes only when its process exits 0. In act mode every job that started must also report `success`. A timeout, a missing job or an error is a failure.
- A result belongs to a git tree, never to a commit. A commit gets a stored pass only when its tree is the tree that ran, so a commit with one more file runs again. The key also holds the config on `main`, the workflows as they run, the tool versions and the images, so a change to any of them runs again ([when a stored result counts](@/docs/how-it-works.md#when-a-stored-result-counts)).
- Only a pass is reused. A failure runs again next time.
- A `paths` skip is worked out again on each run and never stored. Any doubt about which files changed means run.
- With branches required to be up to date, the tested tree is the tree a squash merge puts on `main` ([require the checks](@/docs/branch-protection.md)).

## What it does not prove

- That the tests are good. Tests are part of the PR, so an agent can delete, skip or weaken one and the run passes. The check names changes to CI files and to what a command runs, such as `package.json` scripts, for the reviewer. Review test changes.
- That the code passes on GitHub's runners. Host mode tests this machine's OS and toolchain, and act's images only resemble GitHub's. Your workflows still run on GitHub after each merge to `main`.
- That the suite passed in order, with `shard_tests` on.
- That no one forged a result. shipcheck runs as your user and keeps its results in a folder your user can write. It stops an agent that claims a pass. It does not stop one that sets out to write a fake result there. Keep that folder outside your agent's sandbox ([agents](@/docs/agents.md#your-agent-s-sandbox)).

[Limits](@/docs/limits.md) has the full list.
