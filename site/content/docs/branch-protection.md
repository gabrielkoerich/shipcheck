+++
title = "Require the check"
description = "Make main accept a PR only after its shipcheck check passes."
weight = 25
+++

Once the shipcheck App posts checks on a repository, tell GitHub to require it. Then a PR can merge only after its `shipcheck` check passes, whoever opened it.

The hosted App is not ready yet. Until it is, the check needs your own App with its key on the machine that runs shipcheck ([install](@/docs/install.md#what-works-without-the-github-app)). Below, **shipcheck.run** stands for the App that posts the check.

## With a ruleset (recommended)

In the repository: **Settings**, **Rules**, **Rulesets**, **New ruleset**, **New branch ruleset**.

| Setting | Value |
|---|---|
| Enforcement status | Active |
| Target branches | Include default branch |
| Require status checks to pass | On |
| Status checks | Add `shipcheck`, and set its source to the **shipcheck.run** App |
| Require branches to be up to date before merging | On |

Set the source. Without it, any token with write access could post a check named `shipcheck` and satisfy the rule, an agent's own token included. With it, only checks from the App count, and the App posts only results shipcheck produced.

Require branches to be up to date. An up-to-date branch already contains `main`, so the tree shipcheck tested is the tree a squash merge puts on `main`.

Rulesets also work for an organization, across many repositories at once: **Organization settings**, **Repository**, **Rulesets**.

## With classic branch protection

**Settings**, **Branches**, **Add branch protection rule** for `main`:

1. Turn on **Require status checks to pass before merging**.
2. Turn on **Require branches to be up to date before merging**.
3. Search for `shipcheck`, add it, and pick the **shipcheck.run** App as its source.

Rulesets are GitHub's newer form of the same rule, and the one to use for new setups.

## Who may change what runs

shipcheck reads `shipcheck.toml` and the workflows from `main`, never from the PR. A PR that rewrites its own workflow to pass, or empties `workflows`, is still checked with `main`'s version and fails. Its change takes effect only after it merges.

So the risk is a weaker CI that gets merged. Require a human owner's review for those files with GitHub's CODEOWNERS:

```
# .github/CODEOWNERS
/shipcheck.toml       @your-name
/.github/workflows/   @your-name
```

and turn on **Require review from Code Owners** in the same ruleset.

To help that review, the check's title ends in ", changes CI" when a PR changes those files, and its summary names each job or step the PR lets fail with `continue-on-error` or turns off with `if: false`, and each job or step it removes.

The owner changes CI through a PR like anyone else, or pushes it to `main` directly. GitHub does not let anyone approve their own PR, so a sole owner adds themselves to the ruleset's **bypass list**. GitHub records every bypass.

Give agents their own GitHub identity: a bot account or a fine-grained token, without admin, and not a code owner. An agent that pushes with the owner's login is the owner as far as GitHub can tell, and CODEOWNERS and the bypass list then protect nothing.

## Before you turn it on

A required check that nobody posts blocks every PR. Turn the rule on after the App is installed and a first PR has shown a `shipcheck` check. If a PR waits on `shipcheck` for a long time, the commit's result is missing: run `shipcheck run` on that commit, or check that the hooks are installed in the clone it came from.

## Merge queue

Do not turn on GitHub's merge queue for `main` yet. The queue requires the check on each group it builds, and shipcheck does not post on queue groups yet, so every group would wait. Support is planned: a group whose tree was already tested will pass at once.
