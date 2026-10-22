# How a check is earned

When the `shipcheck` check passes, and what that guarantees. The threat is a cooperative agent that reward-hacks, not an attacker (DESIGN.md).

## Cache key

```
key = hex SHA-256 of these lines, in this order (\0 is a zero byte):
  tree\0<tree id>
  config\0<hex SHA-256 of shipcheck.toml at origin/main>
  workflow\0<path>\0<hex SHA-256 of the file as it runs>     one per workflow at origin/main, sorted by path, listed ones after injection with a fixed nonce, none in host mode
  kind\0<act or host>
  check\0<name>\0<hex SHA-256 of the command>       one per [host] check, sorted by name, none under act
  runner\0<hex SHA-256 of the identity>            act: `act --version`. host: "host", then \0<command>\0<trimmed stdout> per `versions` entry
  shipcheck\0<shipcheck version>                     the injected steps change what runs
  platform\0<label>\0<image@digest>                one per platform, sorted by label
```

`src/cache.rs` pins one key computed from fixed inputs, checked against an independent implementation. The runner identity is hashed so its line cannot hold a newline.

The key holds everything that decides the result and nothing that does not. Two SHAs with the same tree share a result, so a run of the working copy counts for the commit of that tree, and a rebase, a squash or a merge group with an unchanged tree needs no new run. A change to the config on `main` changes the key, so old results stop counting.

Rejected: the commit SHA as the key. Every rebase and every merge group makes a new SHA with the same tree.

## Rules

The daemon posts `success` for a SHA only if all of these hold:

1. The cache entry for the SHA's key was written by the daemon, from a run it started.
2. Each job ran in its own fresh copy-on-write clone checked out at a commit with that SHA's tree, with the workflows and `shipcheck.toml` from `origin/main`.
3. Every job of every workflow in `workflows` exited 0, within the configured retries, and printed a start marker and an end marker with status `success`, or was skipped by its own `if:` (ARCHITECTURE.md, injected steps). The one exception is a job with `continue-on-error: true`, whose failure GitHub does not count either.
4. Every workflow in `workflows` exists on `main`, and every platform image is pinned by digest.

One pass needs no run: when `paths` is set and no file changed since the merge base with `main` matches it. The check then says nothing ran and why. That decision depends on where `main` is, so it is made again each time and never cached. File names are read NUL-separated with rename detection off, so a quoted or moved path cannot slip past, and a name that cannot be read means run.

Anything else is `failure`, including a missing job, a timeout and a daemon error.

With `shard_tests` on, a job's one-line pytest step runs as N pytest processes in the job's container, each with shipcheck's plugin (ARCHITECTURE.md). The host accepts the sharded attempt only after `act` exits 0, no container of the job is left, and the reports in the job's `.git` show the N shards the host asked for, each collecting the same set of node ids, together running each of them once, with no failed item and every shard exiting 0. Any other outcome, including a failing test, reruns the job unsharded with the normal retries, and that run's result is the job's result. So sharding can make a run slower, never fail one that passes serially.

The markers can only make a result stricter. Job code can read the nonce in the workflow copy and print a fake end marker, but a pass still needs exit code 0 from `act`, so a fake `success` cannot turn a failed run into a pass.

## What this guarantees

| Cheat | Result |
|---|---|
| The agent says the tests passed | No effect. The agent cannot post the check, and the daemon trusts only its own runs |
| The agent edits a workflow or `shipcheck.toml` | No effect. The daemon reads both from `main` |
| The agent adds a workflow job | The run fails until `main` lists the job |
| The agent ran tests on a dirty worktree | No effect. Only `shipcheck run` counts, and it tests a tree built from the working copy in its own clone. A result counts for a commit only when the commit's tree is that tree |
| The agent tests, then commits other files | The commit's tree differs, so it has no result and runs again. `shipcheck commit` refuses it |
| A pass from an old `main` config | No effect. The config digest is in the key |

## What it does not guarantee

- Tests are PR code. An agent can delete, skip or weaken a test, and the run will pass. Review catches this, and test count integrity is the later fix.
- With `shard_tests` on, a test that fails serially only because an earlier test left state behind can pass sharded, since its shard may not run that earlier test. The serial hosted run on `main` is the backstop. This is why `shard_tests` is off by default.
- `act` is not GitHub's runner. The hosted run on `main` is the backstop.
- shipcheck sets `prune-cache: false` on setup-uv steps, and moves `pnpm/action-setup`'s `run_install` into steps of its own, in the job's checkout, including local composite actions (ARCHITECTURE.md). Only cache cleanup changes, and the tree and key come from git.
- The App key is readable by the developer's user on Linux. An agent that sets out to read it can post any check.
- Everything under `SHIPCHECK_HOME` is writable by the developer's user, so an agent running as that user can write a passing result for any key. The fix is a separate OS user for the daemon, with its own clone and home, that posts only its own runs.
- State shared between runs is outside the key: the warm template per job, the cache volumes and the tool homes in them. Job code of one commit can leave something there, a patched test runner or a fake `cargo`, that makes a later commit's run pass. Planned fix: warm templates and write volumes only from runs of commits on the base, and give other runs throwaway copies.
- A macOS job mapped to `-self-hosted` runs on the host, not in a container, with the developer's user.

## Host mode

With `runner = "host"` a pass means: every check in `[host] checks` on `main` exited 0, within the retries and the timeout, as a `sh -c` process shipcheck started in its own clone of the tree, with the toolchain whose `versions` output is in the key. Rule 2 holds with checks in place of jobs, and nothing from the base is written into the clone. Rules 3 and 4 do not apply.

It does not mean:

- That the workflows pass. The owner's commands may differ from what CI runs, and GitHub's runner, its images and its setup steps are not involved. The hosted run on `main` is the backstop.
- That the command was not changed. `npm test` runs the branch's `package.json`. The check names such changes since the merge base, in act mode too, where a workflow's `run: npm test` reads the same file (`package.json` scripts, `Makefile`, `justfile`, `pyproject.toml` `[tool]`, `Cargo.toml` outside its dependencies and version, `.cargo/config.toml`), and the reviewer decides. Lockfiles and the scripts those files call are not named.
- That the toolchain is the same, beyond what `versions` prints. Pin files in the tree, such as `rust-toolchain.toml`, `.nvmrc`, `.tool-versions` and `.python-version`, are in the key through the tree. Globally installed tools, the host's libraries and the state of `~` are outside it. `versions` runs in the repository's checkout, 5 s at most per command, and a failure stops the plan.
- That the warm state is clean. The template is warmed from the developer's checkout and from earlier runs, outside the key, like act mode's templates.

## Why the head tree is the merge tree

Stage 1 requires up-to-date branches. An up-to-date head contains `main`, so squash-merging it makes a `main` commit with the same tree as the head. The check on the head therefore covers the tree that lands on `main`.

Stage 2 uses the merge queue. GitHub builds each group head from `main` plus the PRs ahead of it. If the group head's tree equals a tested PR head's tree, the cache answers. Otherwise the daemon runs the group head.

## Manifest

The check output records how the result was reached:

| Field | Value |
|---|---|
| Tree | tree id of the SHA |
| Base | `origin/main` commit the config was read from |
| Config | digest of `shipcheck.toml` and the workflows |
| Environment | `act` version, image digests, daemon version |
| Jobs | per job: exit code, attempts, SHA-256 of the log |
| Shards | per job with a sharded pytest step: shard count and items run, or why it ran again unsharded |
| Source | `cache hit` or `run`, with the run date |

## If the threat changes

If outside contributors or untrusted runners join, the first step is a separate OS user for the daemon and an Ed25519 signed statement per run.
