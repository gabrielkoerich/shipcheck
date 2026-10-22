# shipcheck design

shipcheck turns the test run an agent needs anyway into the GitHub check its PR waits for. A local daemon runs the repository's GitHub Actions workflows with `act` in a warm copy-on-write checkout, caches the result by git tree, and posts a required check from its own GitHub App. A PR whose tree was already tested gets its check at once. Hosted Actions run only on `main`.

[ARCHITECTURE.md](ARCHITECTURE.md) describes the parts. [PROOF.md](PROOF.md) says how a check is earned and what it guarantees. [SECURITY.md](SECURITY.md) says what a job can reach.

## Problem

[orch](https://github.com/gabrielkoerich/orch) agents finish faster than hosted CI can check them. PRs wait about 5 minutes for checks, every push bills Actions minutes, and a red `main` stops every agent. The agent already ran the tests before it pushed. That run should count.

There is no attacker. Agents are cooperative but can reward-hack: report a pass without running, add `exit(0)`, edit a workflow. The design stops that and nothing stronger. Signatures, TEEs and zk proofs would stop stronger cheats, and we do not need them.

## Principle

A deterministic tool reports the result, never the agent. The agent can ask for a run. Only the daemon turns a run into a check, from facts it reads itself: exit codes of processes it started, tree hashes from git, config from `main`.

## Decisions

| Decision | Rejected alternative | Why |
|---|---|---|
| The daemon runs `act` itself | The agent reports its result (`gh-signoff`) | A report is a claim. A run the daemon started is a fact |
| | The daemon watches the agent's own `act` runs | No second run, but the daemon must prove the agent's worktree was clean and unmodified. Later, once the cache makes the second run cheap enough to compare |
| Agents do not run the project's test commands. They run `shipcheck run`, commit all their files with `shipcheck commit -m ...` or `git add -A && git commit`, and work in small steps, each step committed. Every commit is then a tested tree and gets its check from the cache at once | Agents test with their own commands, and shipcheck runs again after the commit | Double work, the old behaviour. The run the agent waits for is the check |
| `shipcheck run` tests the working copy, as a tree built in a copy of the index and a commit no ref names, keyed by tree like any commit | Accept the agent's own test output for the working copy | A report is a claim, the invariants allow only a run shipcheck started. Testing the snapshot keeps that and still saves the second run |
| | Test only commits | The agent tests, commits, and the same tree runs again |
| `shipcheck commit` commits exactly the tested tree, and refuses when a file changed since the run | Tell agents in SKILL.md to commit all of it | Instructions are followed most of the time. A partial commit or a late edit then misses the cache and runs again, so the tool guarantees the match. `--staged` and `status` cover agents that stage by hand |
| Agents learn the rule from a block in AGENTS.md that `init` writes, and the agents' own hooks enforce it: `shipcheck hook` denies test runners and `git commit` and blocks a stop with untested work, once | An MCP server with a run tool | The agent still runs its usual test command, nothing makes it pick the tool, and it is one more process per agent. The CLI already is the contract |
| | Route the project's test commands to shipcheck, such as a `cargo test` wrapper on PATH | It changes what a person's `cargo test` does, needs a shim per tool, and breaks a job that calls the same command inside CI |
| | Instructions only | Agents follow them most of the time. A hook turns the habit into a deny with the command to use instead |
| The check comes from a GitHub App, pinned in the ruleset | A commit status | Any token with repo write can post a status, including the agent's |
| A pre-push hook written by a one-step `shipcheck install` per clone | Rely on agents and prompts to run `shipcheck run` | A team needs it on every clone. Git cannot install hooks on clone, so one setup step is the minimum, and the App gate covers clones that skip it |
| | A hook manager dependency (husky, lefthook, pre-commit) | Ties shipcheck to one ecosystem. `install` works next to them instead |
| A generic CLI and a required check, no code for any one orchestrator | Hooks inside orch | Anyone with any agent or orchestrator can use it, orch included |
| The daemon runs as the developer's user, with no signed statement | A separate OS user and an Ed25519 statement (first draft) | They answer an attacker we do not have, and cost a second user, a second Docker engine and a key ceremony |
| Config and workflows come from `origin/main` | Config from the PR | The agent could edit what runs |
| The repo owner lists the workflows that must pass in `shipcheck.toml` | The daemon runs every job | Deploys and other `main`-only jobs must never run from a laptop |
| Workflows stay unchanged. The daemon adds `workflow_dispatch:` to its run copy and runs `act workflow_dispatch` | The owner adds `workflow_dispatch:` to each workflow | Users should not change their normal workflows to adopt shipcheck |
| | `act push` with a fake push to `main` | Jobs guarded by `if: github.ref == 'refs/heads/main'`, such as deploys, would run locally. Under the injected trigger `github.ref` is the PR branch, so they skip |
| | Add `pull_request:` to the workflows | GitHub would run them on every PR again, the bill we are removing |
| Each run gets a new copy-on-write clone of a warm template, through the `git_cow` crate | `git archive` into an empty directory | Every build starts cold, as slow as hosted CI |
| | Run in the agent's worktree | Dirty files, and races with the agent still working in it |
| Results are cached by tree and config digest | Cache by commit SHA | A rebase or a merge group makes a new SHA with the same tree |
| The daemon polls GitHub for SHAs that need a check and answers from the cache or runs | A webhook receiver | Needs a public URL |
| | The CLI posts the check after push | Two paths that post checks. Polling covers PRs from humans and other tools too |
| GitHub merge queue serializes merges (stage 2) | Our own queue with bisection | GitHub already regroups on failure, and a low failure rate makes bisection rare |
| One retry before a failure counts | Fail on the first red run | One flaky test would bounce a PR back to its agent |
| `shard_tests` splits a one-line pytest step into N pytest processes in the job's container, every N-th collected item per shard, with a plugin built into the binary. Off by default, `shipcheck init` turns it on | Install pytest-xdist into the job | Changes the user's environment and lockfile, and xdist's own scheduling hides which items ran |
| | Shards in separate containers, one job each | Each container repeats setup, about 20 s on bean, and needs its own checkout and slot |
| | Whole files per shard, longest first by item count | Measured on bean: the slow tests share a file, so one shard took 207 s and the step 210 s, against 131 to 150 s with every N-th item. Cost of the split: a module fixture runs once per shard that holds one of its tests |
| | Test selection, such as pytest-testmon | Skips tests by a guess about what changed, so a pass stops meaning the suite passed |
| | On by default | A pass with sharding off means the workflow passed as written. Sharding has a gap a user should choose knowingly: a test broken only by an earlier test's leftovers can pass (PROOF.md) |
| `act` in Docker | Native mode, steps on the host | Native is faster on macOS but drifts from GitHub's runners. Later, as an option per job |
| Two modes per repository, set in `shipcheck.toml` on main. `runner = "act"`, the default, runs the workflows in containers: a pass means the workflows as written passed, isolated from the machine. `runner = "host"` runs commands the owner declares under `[host]`, natively in an OS sandbox, each in its own warm copy-on-write clone: a pass means those commands exited 0 on this machine's toolchain (PROOF.md, SECURITY.md) | Auto-map workflow jobs to host commands | A job's steps assume the runner's image, setup actions and `${{ }}` context. A mapping would guess, and a wrong guess passes or fails for reasons nobody wrote down. `shipcheck init` suggests commands from one-line run steps, commented out, for the owner to confirm |
| | Commands from the CLI, such as `shipcheck run -- npm test` | The agent picks what counts, and `true` passes |
| | Act only | Docker on a Mac costs seconds per job and a VM's memory, and projects with macOS-only tools cannot run in it at all |
| Host checks run in an OS sandbox shipcheck sets up: `sandbox-exec` on macOS, bubblewrap on Linux. A check writes only its clone, a new home and the repository's tool caches, reads only the system and its toolchains, and reaches no Unix socket or macOS service that starts a process outside. Without a sandbox, host mode refuses to run | Run `shipcheck run` inside the agent's own sandbox | shipcheck writes its results under `SHIPCHECK_HOME`, which the agent must not write, or it could write a pass. A run inside the agent's sandbox can neither write the cache nor be trusted by it |
| | No sandbox, as act mode's container is the wall | `shipcheck install` lets agents run `shipcheck run` outside their sandbox for Docker. In host mode that would run the branch's code with the user's rights, so a sandboxed agent would escape through it |
| | Landlock on Linux | Needs a helper binary and kernel 6.12 to scope Unix sockets. bubblewrap is one package |
| Host checks keep the network | Network off, or through a proxy | Package managers download, and a proxy per run is a component to build. The sandbox closes the paths to the machine instead: Unix sockets, Mach services and files |
| `shipcheck init` writes host mode, with checks detected from the root's project files: `package.json` `test` and `lint` scripts with the lockfile's package manager, `cargo test` and clippy for `Cargo.toml`, `pytest` (through `uv run` with `uv.lock`) for `pyproject.toml`, `go test` and `go vet` for `go.mod`, each with its `versions`. It writes act with the workflows when it finds none, and `runner` is always written. A repository without `shipcheck.toml` keeps act's defaults. This waited for the host sandbox, since host mode without it hands branch code the user's rights | act as `init`'s default | act tests the workflows in GitHub's image, which is not where the code runs. When the agent's machine is set up like production, with the same tools, dependencies, OS and system libraries, though not the same CPU, memory or scale, a host check tests the code where it will run, more faithfully than act or GitHub's hosted runner. Host mode also needs no Docker and runs warm checks in seconds. act stays the mode closest to GitHub's CI |
| | Host mode without a file on `main` | With no file the owner has not chosen any commands, and the workflows are the only ones written down |
| | Detect commands from the workflows | A workflow's run steps assume its setup actions and image. Project files name the tool directly |
| Host mode warms each check's template from the developer's checkout | Templates fed only by shipcheck's runs, as under act | Host builds match the host, so the developer's `node_modules` and `target/` are what a first run needs. A Python `.venv` holds absolute paths, so git-cow does not share hidden directories, and `uv sync` rebuilds one from uv's cache |
| Our App's key lives in a hosted service that posts the check for any user with write access to the repository | Give each team the key, or have each team register its own App | One App for every team, and the key never leaves our service |
| The service is a Cloudflare Worker with one D1 table, on the free plan | A container on the author's VPS, deployed with Kamal | The service is idle most of the time. A VPS still needs TLS, uptime watching, the key on a disk and a backup job. The Worker needs none of them |
| Machines sign in with GitHub's device flow on our App, and get a machine token of the service's own that expires after 90 days without a use, so a machine in use never needs a person at the device flow again. Rejected: a fixed lifetime, which stops every agent's posts on the same day until a human signs in | The CLI sends the GitHub user token with every post | The service would keep a token that can act as the user. A machine token can only post the check, and revoking it touches nothing on GitHub. The service accepts only a user token GitHub confirms our App issued, so a leaked PAT cannot sign a machine in |

## Stages

**Stage 1: checks on PR heads.** The daemon checks every open PR head. The ruleset requires the `shipcheck` check and an up-to-date branch. An up-to-date head already contains `main`, so its tree is the tree a squash merge creates. A tool that already waits for required checks needs no change. Agents call `shipcheck run` before they push, and read `shipcheck logs` on failure.

**Stage 2: merge queue.** Requiring up-to-date branches makes every merge force the other PRs to rebase and run again. Warm caches make that run incremental, but the waits add up as agents grow. In stage 2, tools enqueue PRs instead of merging them, and the daemon also checks merge group heads. A group whose tree matches a tested PR head gets its check at once.

**Later:** the watcher, native mode, a cache shared between machines, mergiraf as the merge driver, reuse across a moved base, test count integrity.

## Costs

- Every step an agent commits costs a full warm workflow run, since the run replaces the agent's own test command. Warm speed is what matters most: [BENCHMARKS.md](BENCHMARKS.md) and [the speed table](https://shipcheck.run/#timing-title).
- Untracked files that are not gitignored are part of the tested tree. An agent must gitignore or delete junk, or the commit's tree differs and runs again. `shipcheck run` lists them.
- The agent hooks match command text. `sh -c 'cargo test'`, a script, or a variable gets past them, and an agent can still write the results through a script it runs (SECURITY.md). They steer a cooperative agent, the App gate stays the check.
- Each agent's hook format is its own, and each changes. Five reply shapes to keep current against their docs.
- Codex and Gemini ask the user to trust a repository's hooks before they run, so the first session needs a person.
- Each run of a working copy leaves a commit no ref names. `git gc` prunes them after its usual grace period.

- `act` differs from GitHub's runners. A green local run can fail on `main`. The backstop is a hosted run on every push to `main`, which bills once per merge instead of once per push. Move it to a schedule to cut the bill further.
- On macOS, Docker bind mounts go through virtiofs, which is slow for large build directories. This is the main speed risk, and native mode is the answer if it bites.
- Each run warms the template for the next, including runs of commits that fail. Correctness then rests on the build tool's own incremental cache (cargo, turbo, pnpm), as it does on a developer's laptop. The template state is not in the cache key. Rejected: warm only from `main`, which leaves PR runs colder.
- Toolchains and package caches live in Docker volumes per repo between runs, outside the cache key like the template. A broken toolchain in a volume breaks every run of that repo until the volume is removed (`docker volume rm shipcheck-<owner>-<repo>-...`).
- Some matrix legs cannot run under act at all, such as one that mounts a loop device. The owner then cannot require that workflow, since `workflows` takes whole workflows.
- If the daemon is down, required checks time out and PRs wait. The ruleset timeout fails closed.
- An agent set on cheating can read the App key on Linux, where it is a file owned by the same user. On macOS the Keychain limits it to the daemon binary. We accept this because there is no attacker.
- The cache lives on one machine. Agents that share one machine share the cache. In a team each daemon answers only from its own cache, and one daemon with `run_misses` runs the rest. Rejected: every daemon runs every miss, which runs each PR once per developer. A shared cache comes later if teams need it.
- The hosted service runs within Cloudflare's free plan: 100,000 requests a day, 10 ms of CPU per request, and D1's daily row limits. One post is one request, so this covers tens of thousands of posts a day. Past that, the paid plan costs $5 a month.
- Cloudflare holds the App's private key as a Worker secret. Anyone who controls the Cloudflare account can post any check on every repository that installed the App.
- The service cannot prove a run happened. It posts what a machine of a user with write access sends, as a local key does (PROOF.md). An agent can read a machine token on the developer's disk, as it can read the App key on Linux.
- A sharded run that fails, for any reason, runs the job again unsharded, so a red PR with `shard_tests` on takes the sharded time plus a serial run. A job whose pytest step its own `if:` skips also runs twice, since no reports come back. Each shard collects the whole suite, about 12 s on bean, and holds its own memory.
- Host mode runs branch code as the user, with the network, in an OS sandbox. A check can still send anything it reads to the internet, reach TCP services on the machine and the LAN, and read system files. It is for the developer's own work only, and the daemon refuses it for forks (SECURITY.md).
- Host checks see a new `HOME` and their own `CARGO_HOME`, npm, uv and Go caches. The first run fills those caches, and a cargo run after a warm from the developer's checkout rebuilds the dependencies, since their paths changed. A toolchain manager outside the list in `src/runner/host.rs` cannot be read and fails. A config in the user's home, such as `~/.cargo/config.toml` or `~/.npmrc`, does not apply.
- Host mode's pass is the owner's commands, not the workflows. A branch can change what a command runs, through `package.json` scripts or a `Makefile`, and the check only names it, in both modes.
- Host mode warms from the developer's checkout at most hourly. The run after a warm rebuilds what embeds the checkout's path, such as cargo's workspace crates, since the template's `target/` then holds the developer's fingerprints.
- No macOS or Windows runners under `act`. A workflow with such jobs stays out of `workflows` and runs only on `main`, unless a daemon runs on a matching host.

## Limits of act

- Default images lack many tools from GitHub's runners. Pin full images by digest.
- Docker cannot run systemd, and service containers work only partly.
- Steps that need a real `GITHUB_TOKEN`, OIDC or the Actions cache do not behave as on GitHub.
- `act` copies the work directory into the container by default. The daemon passes `--bind` to keep the copy-on-write gain.
