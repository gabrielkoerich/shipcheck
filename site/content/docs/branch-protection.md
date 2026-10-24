+++
title = "Require the check"
description = "Make main accept a PR only after its shipcheck check passes."
weight = 25
+++

Once the [shipcheck App](https://github.com/apps/shipcheck-run) posts checks on a repository, tell GitHub to require them ([post the checks](@/docs/install.md#post-the-checks)). shipcheck posts one check per job, as GitHub Actions does, and those job checks are the required ones. Then a PR can merge only after every job passes, whoever opened it.

Below, the shipcheck.run App is the one that posts the checks. With [your own App](@/docs/install.md#your-own-app), pick that one instead.

## With a ruleset (recommended)

In the repository: Settings, Rules, Rulesets, New ruleset, New branch ruleset.

| Setting | Value |
|---|---|
| Enforcement status | Active |
| Target branches | Include default branch |
| Require status checks to pass | On |
| Status checks | Add each job check, such as `shipcheck/test` and `shipcheck/lint`, and set the source of each to the shipcheck.run App |
| Require branches to be up to date before merging | On |

Each job's check is `shipcheck/<job>`, named by the job id or the `[host.checks]` name. A job id that two required workflows share gets the workflow file's name without `.yml` or `.yaml` in front, `shipcheck/<stem>/<job>`, for those workflows only. When those workflows' stems are not all different, as with `ci.yml` and `ci.yaml`, the whole file name goes in front instead, `shipcheck/ci.yml/test`, so no two jobs share a check name. `shipcheck init` prints the names and can create this ruleset for you.

A job check is queued until a machine runs the commit, in progress while it runs, then success or failure.

- Each job's check holds the end of its log and how long it took ([the checks on the PR](@/docs/how-it-works.md#the-checks-on-the-pr)). `logs = "failed"` in `shipcheck.toml` keeps the log only on failed jobs.
- A job allowed to fail with `continue-on-error` is neutral.
- A change outside `paths` runs nothing, and every job check is skipped.

GitHub counts success, skipped and neutral as passing a required check ([About protected branches](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches#require-status-checks-before-merging)).

The PR also shows a `shipcheck` check, a summary of the whole run with the cache key. It fails when any job does. Do not require it, the job checks already say the same.

**Set the source.** Without it, any token with write access could post a check named `shipcheck/test` and satisfy the rule, an agent's own token included. With it, only checks from the App count, and the App posts only results shipcheck produced.

Require branches to be up to date. An up-to-date branch already contains `main`, so the tree shipcheck tested is the tree a squash merge puts on `main`.

Rulesets also work for an organization, across many repositories at once: Organization settings, Repository, Rulesets.

## With classic branch protection

Settings, Branches, Add branch protection rule for `main`:

1. Turn on Require status checks to pass before merging.
2. Turn on Require branches to be up to date before merging.
3. Search for each job check, such as `shipcheck/test`, add it, and pick the shipcheck.run App as its source.

Rulesets are GitHub's newer form of the same rule, and the one to use for new setups.

## When jobs change

The required names follow the jobs on `main`. When a job is added, renamed or removed, update the ruleset in the same PR that changes the job, or right after it merges: Settings, Rules, Rulesets, `shipcheck`, then add or remove the names under Status checks.

- A required name that no job posts anymore blocks every PR.
- A new job that is not required can fail without blocking.

`shipcheck doctor` compares the ruleset with the current jobs when your `gh` login can read the branch's rules, and names what to add or remove. It does not read classic branch protection.

## Who may change what runs

shipcheck reads `shipcheck.toml` and the workflows from `main`, never from the PR. A PR that rewrites its own workflow to pass, or empties `workflows`, is still checked with `main`'s version and fails. Its change takes effect only after it merges.

So the risk is a weaker CI that gets merged. Require a human owner's review for those files with GitHub's CODEOWNERS:

```text
# .github/CODEOWNERS
/shipcheck.toml       @your-name
/.github/workflows/   @your-name
```

In the same ruleset, turn on Require a pull request before merging, set Required approvals to 0, and turn on Require review from Code Owners. A PR that touches none of those files then merges with no review, and one that touches them needs a code owner's approval.

To help that review, the check's title ends in ", changes CI" when a PR changes those files, and its summary names each job or step the PR lets fail with `continue-on-error` or turns off with `if: false`, and each job or step it removes.

GitHub does not let anyone approve their own PR, so a code owner's own PR that changes CI waits for another code owner. A sole owner adds themselves to the ruleset's bypass list, then merges such a PR or pushes the change to `main` directly. GitHub records every bypass.

Give agents their own GitHub identity: a bot account or a fine-grained token, without admin, and not a code owner. An agent that pushes with the owner's login is the owner as far as GitHub can tell, and CODEOWNERS and the bypass list then protect nothing.

## Before you turn it on

A required check that nobody posts blocks every PR. Turn the rule on after the App is installed and a first PR has shown the job checks. If a PR's job checks stay queued, the commit's result is missing: run `shipcheck run` on that commit, or check that the hooks are installed in the clone it came from.

## Merge queue

Do not turn on GitHub's merge queue for `main` yet. The queue requires the check on each group it builds, and shipcheck does not post on queue groups yet, so every group would wait. Support is planned: a group whose tree was already tested will pass at once.
