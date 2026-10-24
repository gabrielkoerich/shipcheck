+++
title = "Speed up runs"
description = "Use host mode, shard pytest, let jobs download, keep caches, and pick a Docker engine."
weight = 45
+++

A warm run is often as fast as GitHub's hosted runners or faster. The settings below go in [`shipcheck.toml`](@/docs/config.md) on `main`. The numbers come from runs on an Apple silicon Mac against GitHub's hosted runners, on the same commits.

## Use host mode

`runner = "host"` runs the checks natively, with no container to start. On a Mac, warm runs of git-cow took 2 to 4 s against 7 s under act and about 20 s for the same job on GitHub. express took 3 to 8 s against 37 s under act and about 3.5 min on GitHub. A run in clones costs about 0.6 to 1 s over the same commands run in place.

- Each check's clone starts from a template warmed from your checkout, so a built `target/` or `node_modules` is already there. Keep your checkout built.
- After each hourly warm, the next run rebuilds what embeds your checkout's path, such as cargo's workspace crates. Later runs reuse the run's own build.
- A Python `.venv` is not shared. Put `uv sync` in the check, and it rebuilds from uv's cache in seconds.
- The tool caches start empty, so the first run downloads dependencies once.
- Host mode runs on this machine's OS. orch's tests took 105 s on macOS against 16 s in act's Linux container, so a suite that is slow on your OS can be faster in act mode.

The sections below are for act mode.

## Shard pytest

```toml
shard_tests = true
```

shipcheck splits the pytest step of each job into parallel pytest processes in the job's container, as many as the Docker engine's CPUs and memory allow at 1.5 GiB each, at most 6. `shard_tests = 4` fixes the count. Each process collects the whole suite and runs every N-th test. shipcheck checks that the shards together ran every collected test once and all passed. Anything else, a failing test included, runs the job again whole, and that result counts. Your command, lockfile and dependencies stay as they are, and nothing is installed.

The step must be one line in bash, such as `pytest -v`, `uv run pytest`, `poetry run pytest` or `python -m pytest`, with no `;`, `&&`, pipes, redirects, `$` or pytest-xdist flags. `shipcheck doctor` names the workflows that have one.

The cost: a test that fails only after another test's leftovers can pass sharded, since the two may run in different processes. GitHub's serial run on `main` still catches it. `shipcheck init` turns sharding on when it writes act mode. Set `shard_tests = false` if a pass must mean the suite passed in order.

On bean, a Python repository with about 2,000 tests, 5 shards took the pytest step from 286 s to 131 to 150 s, and the whole run from 333 s to 182 to 197 s.

## Give Docker more memory

Sharding runs out of memory before CPUs. shipcheck counts 1.5 GiB for each pytest process, so Docker Desktop's default 8 GB VM fits 5 shards. Parallel jobs share the same memory: `max_parallel` defaults to the host's CPUs minus 1, capped by Docker's CPUs and by 1 GiB of its memory per job, and Rust or JVM builds side by side can still swap or be killed for lack of memory. Raise it in Docker Desktop, Settings, Resources, Memory, such as 16 GB on a Mac with 32 GB or more. More memory does not speed up one serial job. `shipcheck doctor` says when Docker has under 8 GB or under a third of the Mac's memory.

## Let jobs download

```toml
network = "internet"
```

With the default `none`, a job reaches nothing, so steps that install packages or toolchains fail. With `internet` they work. A job then reaches the local network and services this machine exposes beyond `127.0.0.1`, see [the limits](@/docs/limits.md). Turn it on only for code you trust as much as your own.

## Keep caches between runs

`act.cache_paths` keeps paths in the job container in a Docker volume per repository: `/home/runner/.npm`, bun's cache, `/home/runner/.cache`, which holds uv and pip, and the per-job tool homes in `/home/runner/.shipcheck`. Add the cache of another tool if its lock lives inside the path. Each job's checkout is also a copy-on-write clone of the last one, build output included, so `target/` or `node_modules` start warm.

## Pick a Docker engine

Use Docker Desktop, the engine shipcheck is tested on. OrbStack and colima ran the same trials at about the same speed, with limits: OrbStack sometimes showed a job the files of the previous run at the same path, which shipcheck now fails, and colima shares only your home directory, so the repository and `SHIPCHECK_HOME` must be under it. See [install](@/docs/install.md). Give the engine's VM enough CPUs and memory for `max_parallel` jobs at once, and for the shards of each.
