+++
title = "Quickstart"
description = "Set up a repository once, then each clone once, then work as usual."
weight = 20
+++

## For the repository owner, once

In a clone of the repository, on `main`:

```sh
shipcheck init
```

It writes `shipcheck.toml` and installs the hooks for this clone. When the root has a `package.json` with a `test` or `lint` script, a `Cargo.toml`, a `pyproject.toml` or a `go.mod`, the file runs those checks natively in a sandbox. Open it, keep the checks CI needs, and say when runs should start:

```toml
runner = "host"
on = ["push", "commit"]

[host]
versions = ["node --version", "npm --version"]

[host.checks]
test = "npm test"
lint = "npm run lint"
```

Otherwise it writes `runner = "act"` with every workflow in `workflows`, run in containers. Remove the workflows that must not run on a laptop, such as deploys. [Modes](@/docs/config.md#modes) compares the two.

Commit it and merge it to `main`. shipcheck reads this file and the workflows only from `main`, so a PR cannot change what it must pass. [Every setting](@/docs/config.md).

Your workflows stay as they are. On GitHub they keep running on pushes to `main`, as a backstop after each merge.

To post the `shipcheck` check on PRs, shipcheck needs a GitHub App installed on the repository and its key on the machine that runs shipcheck. The hosted App is not ready yet, so for now this means your own App ([what works without it](@/docs/install.md#what-works-without-the-github-app)). Once the check shows on PRs, [require it on `main`](@/docs/branch-protection.md), so a PR merges only after `shipcheck` passes.

## For each teammate, once per clone

```sh
shipcheck install
```

It installs two hooks: pre-push, and post-commit for repositories that run on commits. Git never installs hooks on clone, so each clone needs this one step. To make it automatic, run it from the repository's usual setup: the `prepare` script in `package.json`, a `just setup` recipe, or a `mise` task.

If the repository already uses husky, lefthook or another hook manager, `shipcheck install` leaves its hooks alone and prints the line to add to them.

## Every day

Commit and push as usual. Nothing waits on shipcheck:

- Each commit or push starts a run in the background, as `on` says.
- A newer commit on the same branch stops the older run.
- A tree that was already tested needs no run at all.
- With the App set up, the PR's `shipcheck` check shows the result, green or red.

To run the workflows in the foreground and wait for the verdict, for example as an agent's test step before it pushes:

```sh
shipcheck run
```

When a run fails, give the agent the logs:

```sh
shipcheck logs
```
