+++
title = "shipcheck.toml"
description = "Every setting of the one file the repository owner writes."
weight = 40
+++

`shipcheck.toml` lives at the repository root. shipcheck reads it from `origin/main` only, so a branch cannot change what it must pass. Its settings follow the inputs of a workflow.

Without it on `main`, `shipcheck run` still works: it runs every workflow GitHub would run for a feature branch, with the defaults below, and says so: those on `pull_request`, and those on `push` whose branch filters, by GitHub's glob rules, take an ordinary branch name such as `fix-login`. Deploys on `push: branches: [main]`, `release/**` or tags stay out, and so does `pull_request_target`. That result is never posted as the `shipcheck` check, which follows only the owner's file.

```toml
workflows = ["ci.yml"]
on = ["push", "commit"]
paths = ["src/**", "Cargo.*"]
timeout_minutes = 30
retries = 1
max_parallel = 4
network = "internet"

[act]
platforms = { "ubuntu-latest" = "catthehacker/ubuntu@sha256:<digest>" }
jobs = { "ci.yml/build" = "catthehacker/ubuntu@sha256:<amd64 digest>" }
cache_paths = ["/root/.shipcheck", "/root/.npm", "/root/.bun/install/cache", "/root/.cache"]

[act.job_homes]
RUSTUP_HOME = "rustup"
CARGO_HOME = { dir = "cargo", path = "bin" }
GOMODCACHE = "gomod"
```

| Setting | Default | Meaning |
|---|---|---|
| `workflows` | none, must be set | Workflow files in `.github/workflows` that must pass. Every job in them runs. Workflows not listed run only on GitHub, on `main` |
| `on` | `["push"]` | When a run starts on its own. `push`: the pre-push hook starts one for each pushed commit. `commit`: the post-commit hook starts one for each new commit, so the result is often ready before the push |
| `paths` | every file | Glob patterns. When a change touches no matching file since it left `main`, the check passes without running and says so |
| `timeout_minutes` | `30` | A workflow that runs longer fails |
| `retries` | `1` | How many times a failed job runs again, in a new checkout, before it counts as failed |
| `max_parallel` | half the CPUs | The most jobs, and so containers, one run starts at once. A job starts once every job in its `needs` has passed |
| `runner` | `"act"` | What runs the workflows. `act` is the only runner for now. Its settings live under `[act]` |
| `network` | `"none"` | What jobs can reach. `none`: nothing, not this machine, the local network or the internet. `internet`: what act's network reaches, which today includes this machine and the local network, see [the limits](@/docs/limits.md). Turn it on only for code you trust as much as your own |
| `logs` | `"none"` | `failed` puts the end of each failed job's log into the `shipcheck` check. A log can hold paths, names or secrets act did not mask, and a check on a public repository is public. Values from the secrets file are replaced with `***`. A fork's PR never gets a log |
| `act.platforms` | `ubuntu-latest` and `ubuntu-24.04` on `catthehacker/ubuntu:act-24.04`, `ubuntu-22.04` on `catthehacker/ubuntu:act-22.04`, pinned by digest | Maps a `runs-on` label to an image, pinned by digest. `-self-hosted`, which act uses to run a job on the machine itself, is refused |
| `act.jobs` | none | An image for one job, as `"<workflow>/<job>"`, when jobs that share a `runs-on` label need different images. For example, an `amd64` image for a job whose tools ship no `arm64` build, on an Apple silicon Mac |
| `act.cache_paths` | the four paths above | Paths in the job container kept in a Docker volume per repository, so package caches survive between runs. Parallel jobs share these volumes, so list only caches whose lock lives inside the path. `/root/.shipcheck` holds the per-job homes of `act.job_homes` |
| `act.job_homes` | `RUSTUP_HOME`, `CARGO_HOME` and `RUNNER_TOOL_CACHE` | Gives each job its own directory under `/root/.shipcheck/<job>/` for each environment variable, so parallel jobs never install into one toolchain. `~/.rustup` breaks when two jobs install into it at once, cargo keeps its lock at the root of `~/.cargo`, and setup actions such as setup-java or setup-zig copy into act's one tool cache for every job. `path` adds a subdirectory to the job's `PATH`. A job that sets the variable itself keeps its value. `job_homes = {}` under `[act]` turns it off |

To get the digest of one architecture of an image:

```sh
docker manifest inspect catthehacker/ubuntu:act-latest
```

Pull an `amd64` image once before the first run (`docker pull --platform linux/amd64 <image@digest>`). Docker runs it under emulation on Apple silicon, which is slower, so keep it to the jobs that need it.

A commit whose message holds `[skip ci]`, `[ci skip]` or `[skip shipcheck]` starts no run, and neither does a commit or push made with `SHIPCHECK_SKIP=1`. A skip never posts a result: the commit has no `shipcheck` check, so a PR that requires it cannot merge until a later commit runs. `shipcheck run` ignores skips. Use `paths` for changes that should pass without running.

Runs started by `on` never block a commit or a push. They run in the background, and a failure turns the PR's `shipcheck` check red. A newer commit on the same branch stops the older run first, like `cancel-in-progress` in a workflow.

shipcheck stops with an error if `workflows` is empty or names a file that does not exist, if a `paths` pattern is invalid, if an image is not pinned by digest, if a cache path is not an absolute path made of letters, digits and `._/-`, or if a `job_homes` entry is not a variable name with relative paths. Unknown settings are an error too, so a typo cannot pass silently.
