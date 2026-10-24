+++
title = "shipcheck.toml"
description = "Every setting of the one file the repository owner writes."
weight = 40
+++

`shipcheck.toml` lives at the repository root. shipcheck reads it from `origin/main` only, so a branch cannot change what it must pass. Its settings follow the inputs of a workflow.

Without it on `main`, `shipcheck run` still works and says so. It runs, with the defaults below, every workflow GitHub would run for a feature branch: those on `pull_request`, and those on `push` whose branch filters, by GitHub's glob rules, take an ordinary branch name such as `fix-login`. Deploys on `push: branches: [main]`, `release/**` or tags stay out, and so do `pull_request_target` and a `pull_request` whose `types` has none of `opened`, `synchronize` and `reopened`, such as `[closed]`. That result is never posted as the PR's checks, which follow only the owner's file.

`shipcheck init` writes host mode with a check for each workflow job that runs on PRs. When no job gives one, it uses the checks of a `package.json` `test` or `lint` script, a `Cargo.toml`, a `pyproject.toml` or a `go.mod` at the root. With none of these it writes act mode with every workflow and `shard_tests = true`. It leaves an existing file as it is. It always writes `runner`, so the file says which mode it uses. A host mode file:

```toml
runner = "host"
on = ["push", "commit"]

[host]
versions = ["cargo --version", "rustc --version"]

[host.checks]
test = "cargo test"
clippy = "cargo clippy --all-targets -- -D warnings"
```

An act mode file:

```toml
runner = "act"
workflows = ["ci.yml"]
on = ["push", "commit"]
paths = ["src/**", "Cargo.*"]
timeout_minutes = 30
retries = 1
max_parallel = 4
network = "internet"
shard_tests = true

[act]
platforms = { "ubuntu-latest" = "catthehacker/ubuntu@sha256:<digest>" }
jobs = { "ci.yml/build" = "catthehacker/ubuntu@sha256:<amd64 digest>" }
cache_paths = ["/home/runner/.shipcheck", "/home/runner/.npm", "/home/runner/.bun/install/cache", "/home/runner/.cache"]
slot_paths = ["/var/lib/apt/lists"]

[act.job_homes]
RUSTUP_HOME = "rustup"
CARGO_HOME = { dir = "cargo", path = "bin" }
GOMODCACHE = "gomod"
```

| Setting | Default | Meaning |
|---|---|---|
| `workflows` | none, must be set with `runner = "act"` | Workflow files in `.github/workflows` that must pass. Every job in them runs. Workflows not listed run only on GitHub, on `main`. Empty with `runner = "host"` |
| `on` | `["push"]` | When a run starts on its own. `push`: the pre-push hook starts one for each pushed commit. `commit`: the post-commit hook starts one for each new commit, so the result is often ready before the push |
| `paths` | every file | Glob patterns. When a change touches no matching file since it left `main`, the check passes without running and says so |
| `timeout_minutes` | `30` | A workflow that runs longer fails |
| `retries` | `1` | How many times a failed job runs again, in a new checkout, before it counts as failed. A failed attempt's log stays as `<job>.attempt-<n>.log`, and `shipcheck run` names it |
| `max_parallel` | the host's CPUs minus 1, at most Docker's CPUs and one per 1 GiB of Docker's memory, at least 1 | The most jobs or host checks one run starts at once. A job starts once every job in its `needs` has passed. The default keeps one CPU free, but the machine is busy during a run of many jobs. Set a lower number to keep more of it free |
| `runner` | `"act"` | `host` runs the `[host]` checks natively in a sandbox. `act` runs the workflows in Docker, with settings under `[act]`. `shipcheck init` writes it, see [modes](#modes) |
| `network` | `"none"` | What jobs can reach. `none`: nothing, not this machine, the local network or the internet. `internet`: the internet, from a Docker network of the run's own. Jobs do not reach services on this machine's `127.0.0.1`, but still reach services it exposes on other addresses, and the local network, see [the limits](@/docs/limits.md). Turn it on only for code you trust as much as your own |
| `shard_tests` | `false` | `true` splits the pytest step of each job into parallel pytest processes in the job's container, as many as the Docker engine's CPUs and memory allow at 1.5 GiB each, at most 6. A number fixes the count, from 1 to 32. It never edits your command or lockfile and installs nothing. A sharded run that fails runs again whole, and that result counts. The cost: a test that fails only after another test's leftovers can pass sharded. See [speed up runs](@/docs/speed.md) |
| `logs` | `"all"` | Which job checks carry the end of their log. `all`: every job, passed or failed. `failed`: only failed jobs. A failed job always carries its log, and no setting hides it. On a public repository every reader of a PR sees this output. See [the checks on the PR](@/docs/how-it-works.md#the-checks-on-the-pr) |
| `daemon` | `true` | `false` keeps the daemon away from this repository: `init` and `install` do not add it, and a running daemon skips it |
| `daemon_authors` | none | GitHub logins whose PRs and pushes the daemon on any machine may run when a PR's head has no result. Each daemon runs a head when its own user or a listed login opened the PR and pushed the head. Any other head only gets a stored result posted. Forks never run |
| `host.checks` | none | With `runner = "host"`: each check's name and its shell command. A name starts with a letter or `_`, then letters, digits, `_` and `-` |
| `host.versions` | none | Commands whose output goes into the cache key, such as `node --version`, so a new toolchain runs again. A failing one stops the run |
| `host.env` | none | Names of variables a check gets from your environment, beyond the base list in [modes](#modes). Names that carry credentials are refused |
| `act.platforms` | `ubuntu-latest` and `ubuntu-24.04` on `catthehacker/ubuntu:runner-24.04`, `ubuntu-22.04` on `catthehacker/ubuntu:runner-22.04`, pinned by digest | Maps a `runs-on` label to an image, pinned by digest. The default images run jobs as `runner` with `sudo`, as GitHub does. An image of your own runs as its own user. `-self-hosted`, which act uses to run a job on the machine itself, is refused |
| `act.jobs` | none | An image for one job, as `"<workflow>/<job>"`, when jobs that share a `runs-on` label need different images. For example, an `amd64` image for a job whose tools ship no `arm64` build, on an Apple silicon Mac |
| `act.cache_paths` | the four paths above, plus each detected language's | Paths in the job container kept in a Docker volume per repository, so package caches survive between runs. Parallel jobs share these volumes, so list only caches whose lock lives inside the path. `/home/runner/.shipcheck` holds the per-job homes of `act.job_homes` |
| `act.slot_paths` | apt's lists, plus each detected language's | Paths kept in a Docker volume per repository, run slot and job, for caches that break when two jobs use them at once: apt's lists, Maven, Gradle, sbt, yarn v1 and nub. Jobs in one slot run one at a time. Each slot keeps its own copy, so disk grows with the slots a repository has used. Set `slot_paths = []`, or a shorter list, to drop them, and `shipcheck clean` to remove the volumes |
| `act.job_homes` | `RUSTUP_HOME`, `CARGO_HOME`, `RUNNER_TOOL_CACHE`, plus each detected language's: pnpm (`npm_config_store_dir`), Composer (`COMPOSER_CACHE_DIR`), Bundler (`BUNDLE_USER_CACHE`), Hex, Mix and rebar3 (`HEX_HOME`, `MIX_HOME`, `REBAR_CACHE_DIR`) | Gives each job its own directory under `/home/runner/.shipcheck/<job>/` for each environment variable, so parallel jobs never install into one toolchain. `~/.rustup` breaks when two jobs install into it at once, cargo keeps its lock at the root of `~/.cargo`, setup actions such as setup-java or setup-zig copy into act's one tool cache for every job, and the package caches lock badly or not at all across containers. `path` adds a subdirectory to the job's `PATH`. A job that sets the variable itself keeps its value. `job_homes = {}` under `[act]` turns it off |

shipcheck adds a language's cache paths, slot paths and homes only when the tested commit has one of its files: a JavaScript lockfile, `go.sum`, `pom.xml`, `build.gradle`, `build.sbt`, `Gemfile.lock`, `composer.lock`, `mix.lock`, `pubspec.lock` or a `.csproj`, among others. A list you set yourself is used exactly as written, with nothing added.

Write a report to `$GITHUB_STEP_SUMMARY` and it appears in one shipcheck comment on the PR ([the report comment](@/docs/how-it-works.md#the-report-comment)). No setting turns it on.

## Modes

`runner = "host"` runs commands you declare, natively on this machine in an OS sandbox, with no Docker. A pass means your code passed on this machine's toolchain, operating system and system libraries. When the machine where the agent runs is set up like production, with the same tools, dependencies, OS and system libraries (not the same CPU, memory or scale), a host check tests the code where it will run. That can be more faithful than act or GitHub's hosted runner, whose Ubuntu image is not production. It is also the fastest mode, and the one `shipcheck init` writes when it finds a check and no job runs a Linux matrix act could run. Matrix jobs, Windows jobs and, off a Mac, macOS jobs then stay on GitHub ([which jobs run where](@/docs/how-it-works.md#which-jobs-run-where)).

`runner = "act"` runs the GitHub workflows in Linux containers with `act`, isolated from the machine. It is the mode closest to GitHub's CI: a pass means the workflows as written passed in a container close to GitHub's runner. It needs Docker, and it is the default when `shipcheck.toml` sets no `runner` or `main` has no `shipcheck.toml`. `shipcheck init` writes it when a job runs a matrix of Linux entries, Docker is running and every job runs on Linux, since host mode would test only this machine's toolchain. To switch a repository, change `runner` and, for act, list the workflows in `workflows`.

```toml
runner = "host"

[host]
checks = { test = "npm test", lint = "npm run lint" }
versions = ["node --version", "npm --version"]
env = ["NODE_OPTIONS"]
```

- Each check runs with `sh -c` in its own copy-on-write clone of the tested tree, never in your worktree, all at once up to `max_parallel`. A check passes on exit 0, and a run passes when every check does.
- The clone starts from a template warmed from your checkout at most once an hour, so gitignored directories such as `node_modules` and `target/` are already there. This needs a volume that clones files, such as APFS, btrfs or XFS. Elsewhere a check gets a plain worktree and starts cold. Hidden directories are shared only when they hold build caches, such as `.next`, `.turbo`, `.gradle` and `.uv-cache`. A Python `.venv` stores absolute paths, so it is not shared: put `uv sync` in the check, and it rebuilds from uv's cache in seconds. `git cow share <dir>` adds a hidden folder to the list and `git config cow.warmInterval 30m` changes how often the template is warmed, for host checks too.
- Each check runs in an OS sandbox: `sandbox-exec` on macOS, bubblewrap on Linux ([install](@/docs/install.md#set-up-linux)). Without one, host mode refuses to run, and `shipcheck doctor` says so with the fix. A check writes only its clone, a new empty `HOME` and `TMPDIR`, and the repository's tool caches in shipcheck's folder, one for runs of `main`'s tree and one for other trees. It reads the system and your toolchains, such as `~/.cargo/bin`, `~/.rustup/toolchains`, `~/.nvm/versions`, `~/.local/share/uv/python`, avm's `~/.avm/bin` and Solana's `~/.cache/solana`, and nothing else in your home. Toolchains found only through `$HOME`, Docker's CLI plugins and Solana's platform-tools, are linked into the new `HOME`. It also gets its own rustup home of links and its own copy of Solana's SBF SDK, since `anchor build` relinks both on every build. It can serve and connect to Unix sockets in its clone, `TMPDIR` and caches, such as a test server on a socket in `TMPDIR`, and cannot reach Docker's socket or any other outside them, or start a process outside the sandbox ([security](@/docs/security.md)).
- shipcheck points `CARGO_HOME`, `npm_config_cache`, `BUN_INSTALL_CACHE_DIR`, `UV_CACHE_DIR`, `XDG_CACHE_HOME`, `GOPATH` and `GOCACHE` into the tool caches, so the first run downloads dependencies once. `RUSTUP_HOME` and `AVM_HOME` point at your installed toolchains, which a check can read but not change. Your `~/.cargo/config.toml`, `~/.npmrc` and other config in your home do not apply.
- A check gets `PATH`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `SHELL` and `TERM` from your environment, the names in `host.env`, and `CI=true`. `PATH` loses every entry in a temp folder or inside a git checkout, so a tool an agent wrote there never runs in place of the real one. Nothing else, so tokens, `SSH_AUTH_SOCK`, `AWS_*` and the like stay out. `host.env` refuses `SSH_AUTH_SOCK`, `GIT_*`, `SHIPCHECK_*`, `AWS_*`, `GOOGLE_*`, `AZURE_*`, any name with `TOKEN`, `SECRET`, `PASSWORD` or `CREDENTIAL`, and names ending in `_KEY`.
- A check has the network this machine has, the internet, the LAN and local TCP services. A timeout kills the check's whole process group.
- The cache key holds the tree, the checks and the output of `versions`, each of which may take 5 s. Pin files in the tree, such as `rust-toolchain.toml` or `.nvmrc`, are in the key through the tree. In both modes the `shipcheck` check names a branch's changes to files that can change what a command does: `package.json` scripts, `Makefile`, `justfile`, `pyproject.toml` tool settings, `Cargo.toml` outside its dependencies, and `.cargo/config.toml`.
- A toolchain in your home outside the sandbox's list cannot be read, so its check fails. Install it under `/opt/homebrew`, `/usr/local` or a listed path, or set its variable in `host.env` to a place outside your home, for example `env = ["RUSTUP_HOME"]`.
- After an hourly warm from your checkout, the next run rebuilds what embeds your checkout's path, such as cargo's workspace crates. Later runs reuse the run's own build.
- Commands come only from `shipcheck.toml` on `main`, never from the CLI or the branch.

`shard_tests`, `network` and `[act]` apply only to act, and setting them with `runner = "host"` is an error.

Host mode runs the branch's code as you, in the sandbox, with your network. Use it only for your own and your agents' work. The daemon never checks a fork's PR in host mode. `shipcheck init` writes host checks from the workflows' run steps, or else the project files it finds, and prints where each came from. Check them before you commit the file.

To get the digest of one architecture of an image:

```sh
docker manifest inspect catthehacker/ubuntu:runner-24.04
```

Pull an `amd64` image once before the first run (`docker pull --platform linux/amd64 <image@digest>`). Docker runs it under emulation on Apple silicon, which is slower, so keep it to the jobs that need it.

A commit whose message holds `[skip ci]`, `[ci skip]` or `[skip shipcheck]` starts no run, and neither does a commit or push made with `SHIPCHECK_SKIP=1`. A skip never posts a result, so a PR that requires the job checks cannot merge until a later commit runs. `shipcheck run` ignores skips. Use `paths` for changes that should pass without running.

Runs started by `on` never block a commit or a push. They run in the background, and a failure turns the PR's job checks red. A newer commit on the same branch stops the older run first, like `cancel-in-progress` in a workflow.

shipcheck stops with an error when:

- `workflows` is empty under act, set under host, names a file that does not exist or names one twice
- `max_parallel` is 0
- an `act.jobs` key is not `"<a file in workflows>/<job>"`
- a `paths` pattern is invalid
- an image is not pinned by digest
- a cache or slot path is not an absolute path made of letters, digits and `._/-`
- a `job_homes` entry is not a variable name with relative paths
- a `shard_tests` number is outside 1 to 32
- host mode has no checks, a check name breaks the rule above, a command is empty, or a `host.env` name is refused
- `[host]` is set under act
- a setting is unknown, so a typo cannot pass unnoticed
