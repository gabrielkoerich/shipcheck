+++
title = "Speed up runs"
description = "Shard pytest, let jobs download, keep caches, and pick a Docker engine."
weight = 45
+++

A warm run is often as fast as GitHub's hosted runners or faster. The settings below go in [`shipcheck.toml`](@/docs/config.md) on `main`. The numbers come from [the benchmarks](https://github.com/gabrielkoerich/shipcheck/blob/main/docs/BENCHMARKS.md).

## Shard pytest

```toml
shard_tests = true
```

shipcheck splits the pytest step of each job into parallel pytest processes in the job's container, as many as the Docker engine's CPUs and memory allow at 1.5 GB each, at most 6. `shard_tests = 4` fixes the count. Each process collects the whole suite and runs every N-th test. shipcheck checks that the shards together ran every collected test once and all passed. Anything else, a failing test included, runs the job again whole, and that result counts. Your command, lockfile and dependencies stay as they are, and nothing is installed.

The step must be one line in bash, such as `pytest -v`, `uv run pytest`, `poetry run pytest` or `python -m pytest`, with no `;`, `&&`, pipes, redirects, `$` or pytest-xdist flags. `shipcheck doctor` names the workflows that have one.

The trade: a test that fails only after another test's leftovers can pass sharded, since the two may run in different processes. GitHub's serial run on `main` still catches it. `shipcheck init` turns sharding on. Set `shard_tests = false` if a pass must mean the suite passed in order.

On bean, a Python repository with about 2,000 tests, 5 shards took the pytest step from 286 s to 131 to 150 s, and the whole run from 333 s to 182 to 197 s.

## Give Docker more memory

Sharding runs out of memory before CPUs. A pytest process with heavy imports takes 0.5 to 1.5 GB, so Docker Desktop's default 8 GB VM fits about 5 shards. Parallel jobs share the same memory: `max_parallel` defaults to half the CPUs, and Rust or JVM builds side by side can swap or be killed for lack of memory. Raise it in Docker Desktop, Settings, Resources, Memory, such as 16 GB on a Mac with 32 GB or more. More memory does not speed up one serial job. `shipcheck doctor` says when Docker has under 8 GB or under a third of the Mac's memory.

## Let jobs download

```toml
network = "internet"
```

With the default `none`, a job reaches nothing, so steps that install packages or toolchains fail. With `internet` they work. A job then reaches the local network and services this machine exposes beyond `127.0.0.1`, see [the limits](@/docs/limits.md). Turn it on only for code you trust as much as your own.

## Keep caches between runs

`act.cache_paths` keeps paths in the job container in a Docker volume per repository: `/root/.npm`, bun's cache, `/root/.cache`, which holds uv and pip, and the per-job tool homes in `/root/.shipcheck`. Add the cache of another tool if its lock lives inside the path. Each job's checkout is also a copy-on-write clone of the last one, build output included, so `target/` or `node_modules` start warm.

## Pick a Docker engine

Docker Desktop is the one shipcheck is tested on, and the one to use. OrbStack and colima ran the same trials at about the same speed, with limits: OrbStack sometimes showed a job the files of the previous run at the same path, which shipcheck now fails, and colima shares only your home directory, so the repository and `SHIPCHECK_HOME` must be under it. See [install](@/docs/install.md). Give the engine's VM enough CPUs and memory for `max_parallel` jobs at once, and for the shards of each.
