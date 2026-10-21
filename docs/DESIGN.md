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
| `act` in Docker | Native mode, steps on the host | Native is faster on macOS but drifts from GitHub's runners. Later, as an option per job |
| Our App's key lives in a hosted service that posts the check for any user with write access to the repository | Give each team the key, or have each team register its own App | One App for every team, and the key never leaves our service |
| The service is a Cloudflare Worker with one D1 table, on the free plan | A container on the author's VPS, deployed with Kamal | The service is idle most of the time. A VPS still needs TLS, uptime watching, the key on a disk and a backup job. The Worker needs none of them |
| Machines sign in with GitHub's device flow on our App, and get a machine token of the service's own that expires after 90 days without a use, so a machine in use never needs a person at the device flow again. Rejected: a fixed lifetime, which stops every agent's posts on the same day until a human signs in | The CLI sends the GitHub user token with every post | The service would keep a token that can act as the user. A machine token can only post the check, and revoking it touches nothing on GitHub. The service accepts only a user token GitHub confirms our App issued, so a leaked PAT cannot sign a machine in |

## Stages

**Stage 1: checks on PR heads.** The daemon checks every open PR head. The ruleset requires the `shipcheck` check and an up-to-date branch. An up-to-date head already contains `main`, so its tree is the tree a squash merge creates. A tool that already waits for required checks needs no change. Agents call `shipcheck run` before they push, and read `shipcheck logs` on failure.

**Stage 2: merge queue.** Requiring up-to-date branches makes every merge force the other PRs to rebase and run again. Warm caches make that run incremental, but the waits add up as agents grow. In stage 2, tools enqueue PRs instead of merging them, and the daemon also checks merge group heads. A group whose tree matches a tested PR head gets its check at once.

**Later:** the watcher, native mode, a cache shared between machines, mergiraf as the merge driver, reuse across a moved base, test count integrity.

## Costs

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
- No macOS or Windows runners under `act`. A workflow with such jobs stays out of `workflows` and runs only on `main`, unless a daemon runs on a matching host.

## Limits of act

- Default images lack many tools from GitHub's runners. Pin full images by digest.
- Docker cannot run systemd, and service containers work only partly.
- Steps that need a real `GITHUB_TOKEN`, OIDC or the Actions cache do not behave as on GitHub.
- `act` copies the work directory into the container by default. The daemon passes `--bind` to keep the copy-on-write gain.
