# Benchmarks

Time to result for one commit, shipcheck against GitHub's hosted runners, both started at the same moment.

## Method

- Machine: Apple silicon Mac, 14 CPUs, 36 GB, Docker Desktop. The Docker column gives the VM's memory.
- Repository: bean, the author's private Python repository. Its `ci.yml` runs one job: apt packages, setup-python, setup-uv, `uv sync`, basedpyright and pytest with about 2,000 tests.
- Each round commits a one-line comment to a `.py` file, pushes the branch so GitHub's CI starts, and starts `shipcheck run` at the same moment. shipcheck's time is from its start to its result. GitHub's is from the run's creation to its completion, from `gh run list`.
- Cold is the first run on the machine. Warm is a later run of a new commit, with shipcheck's warm checkout and cache volumes in place.
- A tree already tested, after a rebase or a squash, answers from shipcheck's cache without a run.

## Results

| Round | Docker | shipcheck | GitHub |
|---|---|---|---|
| Cold, 2026-10-21 | 8 GB | 716 s, attempt 1 failed and the retry passed | 367 s |
| Warm, 2026-10-21 | 8 GB | 468 s | 316 s |
| Warm, round 1, 2026-10-22 | 8 GB | 341 s | 356 s |
| Warm, round 2, 2026-10-22 | 8 GB | 275 s | 331 s |
| Warm, `shard_tests` off, 2026-10-22 | 8 GB | 333 s | 411 s |
| Warm, `shard_tests = true`, whole files per shard, 6 shards, 2026-10-22 | 8 GB | 258 s | cancelled by the next push |
| Warm, `shard_tests = true`, every 5th test, 5 shards, 2026-10-22 | 8 GB | 197 s | cancelled by the next push |
| Warm, `shard_tests = true`, every 5th test, 5 shards, 2026-10-22 | 8 GB | 182 s | cancelled by the next push |
| Warm, `shard_tests = true`, every 6th test, 6 shards, 2026-10-22 | 16 GB, 14 CPUs | 162 s | 345 s |
| Warm, `shard_tests = true`, every 6th test, 6 shards, 2026-10-22 | 16 GB, 14 CPUs | 195 s | 337 s |
| Warm, `shard_tests` off, 2026-10-22 | 16 GB, 14 CPUs | 274 s | 405 s |
| Tree already tested | | 0.13 s | a full run |

The warm rounds of 2026-10-22, step by step:

| Step | shipcheck, round 1 | shipcheck, round 2 | GitHub, round 1 | GitHub, round 2 |
|---|---|---|---|---|
| Setup: apt, setup-python, setup-uv, `uv sync` | 24 s | 20 s | 114 s | 70 s |
| basedpyright | 15 s | 16 s | 25 s | 38 s |
| pytest | 289 s | 226 s | 189 s | 192 s |
| Total | 341 s | 275 s | 356 s | 331 s |

Setup went from 168 s to about 20 s. `apt-get update` checks lists kept in a volume instead of downloading 36 MB of them. setup-uv no longer empties uv's cache at the end of each job, so `uv sync` installs 215 packages from it in about 5 s instead of downloading them. Where it still loses is pytest, by 35 to 100 s. Most of that is four tests that call the Hyperliquid API many times in a row, and those calls take longer from this machine than from GitHub's data centre. Running pytest from the container's own disk instead of the bind mount took the same time, 273 s against 267 s. A hook that starts the run at `git commit` gains the push time, about 2 s here.

## Test sharding

`shard_tests` splits bean's `uv run pytest -v` into parallel pytest processes in the job's container (PROOF.md, ARCHITECTURE.md). The sharded rounds above ran back to back, so each push cancelled GitHub's run of the one before. The run with sharding off is the same-day comparison.

| Step, warm, Docker 8 GB | Off | Whole files, 6 shards | Every 5th test, 5 shards |
|---|---|---|---|
| Setup | 20 s | 20 s | 18 to 21 s |
| basedpyright | 14 s | 14 s | 14 to 15 s |
| pytest | 286 s | 210 s | 131 to 150 s |
| Total | 333 s | 258 s | 182 to 197 s |

At Docker 16 GB with 14 CPUs the host rule picks 6 shards, with no push cancelling GitHub's runs:

| Step, warm, Docker 16 GB | Off | Every 6th test, 6 shards |
|---|---|---|
| Setup | 18 s | 18 to 25 s |
| basedpyright | 15 s | 15 to 16 s |
| pytest | 225 s | 108 to 148 s |
| Total | 274 s | 162 to 195 s |

The container peaked at 2.2 GB with sharding off and 4.1 to 4.2 GB with 6 shards. More memory alone did not change the serial run much: 274 s at 16 GB against 275 to 341 s at 8 GB.

- Collection alone takes 12 s: 2,015 of 2,019 tests, 4 deselected by `-m 'not integration'`. Every shard pays it.
- With whole files per shard, one shard took 207 s and the other five 11 to 38 s. The slow tests that call the Hyperliquid API share a file. Every 5th test spreads them, and the slowest shards then took 118 to 149 s.
- A manual run of `uv run --with pytest-xdist pytest -n 6` in a `python:3.13` container on the same engine took 151 s, with no test failing in parallel. Sharding matches it without installing xdist.
- The job container peaked at 3.6 GB with 6 shards and 2.8 to 3.0 GB with 5, by `docker stats` every 2 s.


## Runners on the same machine: git-cow

git-cow's `ci.yml` (fmt, clippy, tests of a crate with no dependencies) under four runners, 2026-10-22. The question: is act slower than GitHub's own runner on the same machine, and how does a local run compare with GitHub hosted from the same start.

- Machine: Apple M4 Max, 36 GB, Docker Desktop 29.7.2 with 14 CPUs and 15.6 GiB.
- shipcheck from `main` with act 0.2.89, a fresh `SHIPCHECK_HOME` for the cold round, base branch with `shipcheck.toml` (`network = "internet"`).
- Gitea's runner v5.2.0 (`gitea-runner exec`, the renamed act_runner), the same pinned `catthehacker/ubuntu` image as shipcheck, no Docker socket, its own network, a fresh `HOME` for the cold round.
- GitHub's actions/runner 2.338.0 in one container from `ghcr.io/actions/actions-runner` (linux/arm64, pinned by digest), no Docker socket, no host mounts, registered as a non-ephemeral self-hosted runner. The image has no C compiler, so `build-essential` went in once before registration, 30 s, not counted below. Its time runs from GitHub's run creation to the job's end, so it includes GitHub's dispatch.
- GitHub hosted: the `ubuntu-latest` ext4 leg of the runs that the shipcheck and Gitea pushes started, from run creation to that leg's end.
- Only the ext4 leg is compared. act skips the macOS leg and the XFS leg needs `sudo mount -o loop`, so the local runners ran a copy of `ci.yml` with only the ext4 leg, and the self-hosted branch sent only that leg to the container.
- Each round committed a comment to `src/lib.rs`, pushed and started the local run at once. Step times from GitHub's API and Gitea's JSON log are whole seconds.

| Runner | Cold | Warm 1 | Warm 2 |
|---|---|---|---|
| shipcheck (act) | 30 s | 7 s | 7 s |
| Gitea runner (act) | 60 s, a second cold run 30 s | 24 s | 24 s |
| GitHub actions/runner in Docker, self-hosted | 31 s | 14 s | 16 s |
| GitHub hosted, same pushes as shipcheck | 38 s | 21 s | 33 s |
| GitHub hosted, same pushes as Gitea | 20 s | 12 s | 21 s |

The warm rounds, step by step. GitHub hosted gives the range of its four runs paired with the warm local rounds.

| Step | shipcheck | Gitea runner | actions/runner, self-hosted | GitHub hosted |
|---|---|---|---|---|
| `dtolnay/rust-toolchain` | 1.9 s, 1.7 s | 19 s, 18 s | 3 s, 3 s | 0 to 10 s |
| `cargo fmt --check` | 0.1 s, 0.1 s | 0 s, 0 s | 0 s, 0 s | 0 to 4 s |
| `cargo clippy` | 0.2 s, 0.2 s | 2 s, 3 s | 1 s, 1 s | 2 to 12 s |
| `cargo test` | 0.9 s, 0.9 s | 1 s, 1 s | 1 s, 1 s | 2 to 3 s |
| The rest: container, checkout, dispatch | 3.9 s, 3.7 s | 1 s, 1 s | 9 s, 11 s | 7 to 9 s |
| Total | 7.0 s, 6.6 s | 24 s, 24 s | 14 s, 16 s | 12 to 33 s |

act runs the steps as fast as GitHub's own runner on this machine: fmt, clippy and test take one to three seconds under each. The warm totals differ by what each runner keeps between runs. Gitea starts a new container and installs Rust each time, about 18 s, the self-hosted runner keeps rustup in its container, and shipcheck keeps it in a volume and keeps `target/`. GitHub hosted took 12 to 38 s for the same leg, and shipcheck's warm runs answered in 7 s.

Earlier partial numbers on bean, the same Gitea command at 14 CPUs and 16 GB, stopped before the other runners ran: Gitea cold 395 s, warm 292 s and 285 s, against shipcheck warm 274 s with `shard_tests` off at the same config.

## Small Linux box

shipcheck on a 1-CPU VPS that also serves a small app, against GitHub's hosted runner, 2026-10-22. The questions: how fast is it on a box this small, and do the defaults behave.

- Machine: rented VPS, Ubuntu 24.04.5, Linux 6.8 x86_64, 1 vCPU, 3.9 GB RAM with about 2.8 GB available, 2.5 GB swap, XFS root with reflink and 61 GB free, Docker 29.9.2, git 2.43.
- It kept serving a Kamal app the whole time: web, api and worker containers, Postgres, kamal-proxy, and cadvisor, beszel-agent, node-exporter and dozzle-agent. All stayed up.
- shipcheck: the linux x86_64 binary of `main` at `0e507de`, built on GitHub. act 0.2.89 is the release binary, checked against its checksum. It ran as a separate user in the `docker` group, with a new `SHIPCHECK_HOME` and every run under `nice -n 10 ionice -c3`.
- Repositories:

  | Repository | Workflow | Commit | Jobs |
  |---|---|---|---|
  | git-cow | `ci.yml`, ext4 leg only (as above) | `a62cb66` | 1 |
  | gin | `gin.yml`, Linux legs | `0f09c3a` | lint and 10 test jobs |
  | express | `ci.yml`, Linux legs | `a419aa7` | lint and Node 18 to 26 |

  Each commit's GitHub run passed.
- Each repository got a local base branch with `shipcheck.toml` (`network = "internet"`), read through `--base`. Cold is the first run of that repository on the machine. Warm 1 and warm 2 each add a one-line comment to a source file, so the tree is new. Each round is `shipcheck run --sha HEAD`.
- Memory: `free -m` and `docker stats` sampled every 2 s.
- GitHub: the Linux jobs of the same commit's run. Wall time is from the run's creation to the end of its last Linux job. Job time is the sum over those jobs.

| Repository | Cold | Warm 1 | Warm 2 | GitHub wall | GitHub job time |
|---|---|---|---|---|---|
| git-cow | 103 s | 19 s | 20 s | 23 s | 20 s |
| gin | 2,579 s | 513 s | 519 s | 105 s | 572 s, 11 jobs of 19 to 74 s |
| express | 1,235 s | 321 s | 318 s | 148 s, 96 s of it queued | 242 s, 10 jobs of 19 to 28 s |

What the defaults chose: `max_parallel` is half the CPUs, at least 1, so 1. `SHIPCHECK_MAX_RUNS` is a quarter of the CPUs, at least 1, so 1. Every job ran alone, which `docker stats` confirms. No pytest step ran, so nothing was sharded.

Memory stayed far from trouble. No job was killed and no container ran out of memory. Swap in use went from 14 MB before the session to 218 MB after it.

| Repository | Peak job container, cold | Peak job container, warm | Lowest available, cold | Lowest available, warm |
|---|---|---|---|---|
| git-cow | 199 MiB over all three rounds | | 2,582 MB over all three rounds | |
| gin | 1,431 MiB, golangci-lint | 444 MiB | 1,229 MB | 2,295 MB |
| express | 409 MiB | 368 MiB | 1,795 MB | 2,176 MB |

- A job on this box is as fast as a job on GitHub's runner. Warm gin test jobs took 17 to 48 s here against 19 to 74 s on GitHub, and warm express jobs 9 to 27 s against 19 to 28 s. The difference is parallelism. GitHub runs 10 or 11 jobs at once, and this box runs one at a time.
- So one warm run here takes about as long as the sum of GitHub's job time: 513 s against 572 s for gin, and 320 s against 242 s for express. That is 2 to 5 times GitHub's wall time. For a single-job workflow like git-cow it is even: 19 s against 23 s.
- Each job costs about 9 to 12 s here outside its container: the copy-on-write checkout, act's start and the container's start. git-cow's warm steps took 10 s of its 19 s. On the Mac with 14 CPUs the same job took 7 s.
- Cold is slow, because every job installs its own toolchain into its own cache volume. In gin, lint (golangci-lint) took 332 s and the first Go 1.27 job 379 s. Each other test job took 165 to 245 s. express's cold jobs took 90 to 132 s each.

What broke:

- `timeout_minutes` defaults to 30 and covers the whole workflow. gin's cold run on one CPU needs 43 minutes, so the first full attempt timed out with four jobs left. The benchmark set `timeout_minutes = 120`. express's cold run took 21 minutes and fit.
- `actions/setup-go` with `go-version: "^1"` takes the newest Go already in the runner's tool cache. GitHub's image has Go 1.26.8, and the catthehacker image has none, so here it downloaded Go 1.27.3. golangci-lint 2.11, built with Go 1.26, then panics on that toolchain. The base branch pinned lint's Go to 1.26.
- act runs the real `actions/checkout` when its `with:` sets `ref: ${{ github.ref }}`, as gin's test job does. With no token, that fails: `Input required and not supplied: token`. Without `ref`, act uses the local checkout. The base branch dropped the `ref`.
- Two gin tests fail here and pass on GitHub. The base branch skipped them with `GOFLAGS=-skip=...`.
  - `TestSaveUploadedFileWithPermissionFailed` expects a permission error, and the job runs as root.
  - `TestRunEmpty` binds `:8080`. With `network = "internet"`, act gives the job the host's network, and cadvisor already listens on 8080 there.
- The same host network means a job on this box reaches every port the host listens on, including its Tailscale address and so the tailnet. Firewall rules for Docker's bridge networks do not apply, since the job's traffic leaves from the host's own network. This is the open risk under `network = "internet"` in SECURITY.md.
- `shipcheck logs HEAD` reads `shipcheck.toml` from `origin/main` and has no `--base`, so after a run with `--base` it said there were no logs. The logs were in `SHIPCHECK_HOME/logs`.
- Also removed on the base branches: the macOS and Windows legs, which act cannot run, gin's Codecov upload, which needs OIDC, and express's artifact upload and Coveralls job.

## Host mode

`runner = "host"` on the same Mac, 2026-10-22, against the act and GitHub numbers above and from the current benchmark. Another agent was running Docker benchmarks at the same time: load average 7 to 16 on 14 CPUs during every round, so treat these as upper bounds.

- shipcheck from the `host-runner` branch, release build, a fresh `SHIPCHECK_HOME`, config on the local base branch through `--base`.
- The developer's checkout was built first, as a developer's is: `cargo clippy` and `cargo test` for git-cow, `npm install` for express. The first run warms each check's template from it.
- Each warm round commits a comment to `src/lib.rs` or `index.js` and runs `shipcheck run --sha HEAD`. Times are shipcheck's own, from start to result, checks in parallel.
- git-cow: `fmt = "cargo fmt --check"`, `clippy = "cargo clippy --all-targets -- -D warnings"`, `test = "cargo test"`, `versions = ["cargo --version", "rustc --version"]`. Rust 1.99.
- express: `test = "npm test"`, `lint = "npm run lint"`, `versions = ["node --version", "npm --version"]`. Node 24.4.1, mocha's 1,261 tests.

| Repository | First run, warmed from the checkout | Warm runs | Same commands in the checkout, one after another | shipcheck act, warm | GitHub hosted |
|---|---|---|---|---|---|
| git-cow | 3.5 s | 4.0 s, 3.5 s, 3.8 s | 2.9 s, 2.9 s | 7 s | about 54 s for the workflow, about 20 s for its ext4 leg |
| express | 4.4 s | 7.9 s, 4.6 s, 4.1 s, 4.6 s, 4.9 s, and three flaky rounds below | lint 0.8 s, test 2.3 s | 37 s | about 3.5 min |
| A tree already tested | 0.26 s | | | | a full run |

The run in clones costs about 0.6 to 1 s over the same commands in place: three clones, a standalone `.git` each, and warming the templates after.

express's tests were flaky on the loaded machine in 3 of 8 warm rounds. One round passed all 1,261 tests and then mocha never exited, since its `npm test` has no `--exit`, until the 5 minute timeout. One failed a test on mocha's 2 s timeout, hung the same way, and hit the 1 minute timeout. One failed a test with 405 instead of 500 and passed on the retry, 11.1 s. Outside shipcheck the suite also hung in two of four runs by hand, so the hang is express's own suite, not host mode's.

express's `res.sendFile` tests fail when the checkout's path has a component starting with `.`, since `send` refuses dotfiles, so a `SHIPCHECK_HOME` under a hidden directory breaks them. The default home, `~/Library/Application Support/shipcheck`, has none.

### Host mode in the sandbox

The same repositories, release builds of `main` at 419e911 (no sandbox) and of the `host-sandbox` branch, on the same Mac, 2026-10-22, one after the other, each with a fresh `SHIPCHECK_HOME`. Another agent ran Docker benchmarks at the same time: load average 11 to 15 on 14 CPUs. express's check is `npm test -- --exit` with a 2 minute timeout, so mocha's hang (above) cannot stall a round.

| Repository | First run, no sandbox | First run, sandbox | Warm runs, no sandbox | Warm runs, sandbox |
|---|---|---|---|---|
| git-cow | 3.8 s | 4.9 s | 2.3, 2.3, 2.1, 2.1 s | 1.9, 2.0, 2.1, 2.3 s |
| express | 7.5 s | 3.7 s | 3.1, 3.5, 3.6, 3.3 s | 3.4, 3.5, 6.8 s, and one failed round |

- Warm runs cost the same within the noise of the loaded machine.
- git-cow's first sandboxed run rebuilds the crate, since cargo sees a new `CARGO_HOME`. A crate with dependencies also rebuilds them on the first run and after each hourly warm from the developer's checkout, whose `target/` was built against `~/.cargo/registry`.
- express's tests failed both attempts in 2 of 10 sandboxed rounds, and the first attempt in a third, with `socket hang up`, a wrong status or a missing header, each time a different test. That is the flakiness under load described above, also seen without the sandbox in the earlier rounds. The failed rounds wrote nothing outside the jail.

## Public repositories

Well-known public repositories, one per language, 2026-10-22. We cannot push to them, so each row compares a recent successful GitHub run on the default branch against shipcheck on the same commit.

- Machine: Apple silicon Mac, 14 CPUs, Docker Desktop 29.7.2 with 14 CPUs and 15.6 GiB, act 0.2.89.
- Each repository got a local base branch with `shipcheck.toml` (`network = "internet"`, defaults otherwise) and the workflow GitHub ran, reduced to its Linux jobs, since shipcheck fails a workflow with a macOS or Windows job. Other edits are listed per row.
- GitHub's total runs from the run's creation to the end of the last Linux job, so it includes the queue, given in brackets.
- Cold is `shipcheck run --sha <commit>` with a new `SHIPCHECK_HOME`. Each warm run adds a comment line to one source file and tests the working copy, so the tree is new and nothing answers from the result cache.
- main is shipcheck at `0e507de`. PR 8 is the `languages` branch with its package caches, each in a home of its own.
- From express on, each run waited until 12 requests of GitHub's anonymous API quota were left, see "Where it loses".

| Repository | Stars | Workflow, jobs | GitHub (queue) | Cold, main | Warm, main | Cold, PR 8 | Warm, PR 8 |
|---|---|---|---|---|---|---|---|
| [pallets/flask](https://github.com/pallets/flask/actions/runs/37633458715) | 74,949 | `tests.yaml`, 9 | 36 s (2 s) | 289 s | 63, 64 s | 320 s | 71, 71 s |
| [BurntSushi/ripgrep](https://github.com/BurntSushi/ripgrep/actions/runs/30916620469) | 69,332 | `ci.yml`, 8 | 78 s (3 s) | 284 s | 84, 73 s | 263 s | 72, 74 s |
| [junegunn/fzf](https://github.com/junegunn/fzf/actions/runs/39693423587) | 83,721 | `linux.yml`, 1 | 378 s (2 s) | 462 s | 381, 385 s | 456 s | 375, 374 s |
| [expressjs/express](https://github.com/expressjs/express/actions/runs/37958916431) | 69,575 | `ci.yml`, 10 | 148 s (96 s) | 220 s | 37 s, failed | 281 s | failed, failed |
| [colinhacks/zod](https://github.com/colinhacks/zod/actions/runs/39337237164) | 44,170 | `test.yml`, 5 | 177 s (2 s) | 651 s | failed, 588 s | failed | 402, 388 s |
| [faker-ruby/faker](https://github.com/faker-ruby/faker/actions/runs/39144020003) | 11,944 | `ruby.yml`, 6 | 29 s (4 s) | 106 s | 67, 68 s | 106 s | 60, 63 s |

Edits to the base branch, and why:

- flask: dropped "Development Versions", which installs Werkzeug's main branch, whose new deprecation warning fails the tests today.
- ripgrep: dropped the cross-compiled legs, since `cross` needs the Docker socket, and GitHub's total covers the same 8 jobs.
- express: dropped the coverage upload and the coverage job. act 0.2.89's artifact server refuses `upload-artifact` v7 (`unknown field "mime_type"`).
- faker: dropped the `head` and `truffleruby-head` legs, whose `continue-on-error: ${{ endsWith(...) }}` shipcheck does not evaluate yet.

Failed runs, by cause:

- express, warm: `listen(0)` fails with `EADDRINUSE` in about 1,000 tests per job. With `network = "internet"` act puts every job on the Docker VM's host network, whose ephemeral range is 55000 to 65535, so 9 test jobs that start together share 10,500 ports. Cold runs pass because installs spread the jobs out.
- zod: `nub install` failed on registry stream errors (`error decoding response body`), in the cold PR 8 run on 3 of 5 jobs. Each warm run on main downloaded all 1,093 packages again. PR 8 keeps nub's store in a slot volume, which is why its warm runs pass and are faster.

Skipped: gin-gonic/gin (89,413 stars): its tests check a permission error that root never gets, bind fixed ports that collide on the shared network, and its Codecov step needs an OIDC token. fzf replaces it for Go. jekyll/jekyll (51,726 stars): its JRuby legs need Java, which the image lacks, and its cucumber scenarios that rename the site's source directory fail with `getcwd ENOENT` on the bind mount. faker replaces it for Ruby.

Steps with the largest differences, warm, the slowest job of each step:

| Step | GitHub | Warm, main | Warm, PR 8 |
|---|---|---|---|
| zod `nub install --frozen-lockfile` | 12 s | 233 s | 144 s |
| zod `nub add typescript@<version>` | 3 to 4 s | 30 to 112 s | 94 to 107 s |
| ripgrep "Build ripgrep and all crates" | 107 s | 9 s | 7 s |
| fzf "Install packages", apt | 12 s | 25 s | 17 s |
| fzf "Unit test" | 5 s | 4 s | 1 s |
| fzf "Integration test", tmux | 306 s | 324 s | 322 s |

Where shipcheck loses:

- Short workflows. flask and faker finish on GitHub in 30 to 40 s, and a warm shipcheck run takes 60 to 70 s. Starting containers, the warm checkout and the second wave of jobs, since `max_parallel` is half the CPUs, cost more than the tests.
- `node_modules` on the macOS bind mount. zod's install takes 140 s with every package in the store, against 12 s on GitHub. A new volume at `node_modules` made it 20 s, but `nub add` then fails with `EBUSY`, since it removes the directory, which is a mount point.
- GitHub's anonymous API quota, 60 requests an hour per address. Setup actions such as setup-node and setup-python read version manifests through it, with an empty token, so a cold run of express takes about 11 requests, and other jobs on the same network share the quota. Runs failed on it until the benchmark waited for quota.
- Cold runs, by 1.2 to 8 times: images, toolchains and packages download once.

Where it wins: ripgrep's build reuses the warm `target/` and finishes in 72 to 84 s against 78 s, and express needs no queue, which took 96 s of GitHub's 148 s. In this round PR 8 cut zod's warm run from 588 s to about 395 s, and faker's and fzf's by a few seconds. flask's warm runs were 7 s slower with PR 8, which then mounted every language's caches in every job.

### Rerun after per-job networks and caches by language, 2026-10-22

main at `a6563b5`, which gives each internet job a network of its own, against PR 8 at `c7e822b`, which mounts a language's caches only when the tree has its files. Same machine and method, new homes, warm runs only.

| Repository | Warm, main | Warm, PR 8 |
|---|---|---|
| flask | 42, 44 s | 38, 49 s |
| zod | 513, 368 s | 396, 409 s |

- flask mounts the same volumes under both, so the two columns differ by noise only.
- zod's `nub install` took 47 to 229 s per job on main, downloading every package, and 138 to 145 s on PR 8, copying from nub's store to the bind mount. With its own network the download no longer failed, and on main's faster run it beat the copy. PR 8's gain on zod is now steadiness, not a lower best time.

