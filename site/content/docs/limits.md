+++
title = "Limits"
description = "What shipcheck does not catch, and what act cannot run."
weight = 70
+++

## What shipcheck does not catch

- Weakened tests. Tests are part of the PR. An agent can delete, skip or loosen a test and the run passes. Review test changes.
- Differences from GitHub's runners. Host mode runs your checks on this machine's OS and toolchain, so a test that fails only on Linux can pass on a Mac. Four of orch's tests failed on macOS in host mode. act runs your steps in Docker images that resemble GitHub's runners but are not the same. GitHub still runs your workflows on `main` after each merge, which catches what slips through. Jobs run as `runner`, uid 1001, with `HOME=/home/runner` and `sudo` without a password, as on GitHub, so a step that needs root needs `sudo` here too. The workspace path, the tool cache and some environment variables differ.
- Test pollution, with `shard_tests` on. A test that fails serially only because an earlier test left state behind can pass when the two run in different shards. GitHub's serial run on `main` catches it. Leave `shard_tests` off if a pass must mean the suite passed in order.
- An agent that writes into shipcheck's folder. Results live in `SHIPCHECK_HOME`, which your user can write. shipcheck does not stop an agent from writing a fake result there. Only the agent's own sandbox does, when it is on and the folder is outside its writable paths.
- An agent that skips the instructions. It runs the project's tests and `git commit` as usual. The git hook then runs the commit again on push, or on commit with `on = ["push", "commit"]`, which costs time, never a false pass.
- A determined cheat on the same machine. shipcheck runs as your user. It stops an agent that claims a pass. It is not built to stop someone who sets out to forge one.

## Windows

shipcheck does not support Windows yet. Host mode needs an OS sandbox (`sandbox-exec` on macOS, bubblewrap on Linux), and shipcheck has none for Windows. It does not build on Windows today and has not been tested there. Until it does, Windows jobs stay on GitHub Actions, and `init` leaves them there.

## What act cannot run

- macOS and Windows jobs. Docker runs only Linux jobs, and shipcheck never runs a job on the machine itself, so a workflow with macOS or Windows jobs fails with `no image for its runs-on label on this host`. Leave it to GitHub, as `init` does ([which jobs run where](@/docs/how-it-works.md#which-jobs-run-where)). Host mode runs macOS jobs on a Mac. The same message for a Linux label, such as `ubuntu-24.04` on act 0.2.89, means the label needs an image in `[act] platforms`.
- The network, by default. With `network = "none"`, the default, a job reaches nothing: no package downloads, no toolchain downloads from setup actions, and no artifacts passed between jobs. With `network = "internet"` all of that works. A job does not reach services on this machine's `127.0.0.1`, but still reaches services listening on its other addresses, and the local network. Blocking those while keeping the internet is planned.
- Reusable workflows. A job with `uses: ./.github/workflows/x.yml` or `uses: owner/repo/.github/workflows/x.yml@ref` fails the run, since act would run the called workflow's jobs without shipcheck's checks, Docker options included. Leave such a workflow out of `workflows` and let GitHub run it.
- Git LFS files and submodules. shipcheck checks out a commit on this machine with git-lfs and submodules turned off, since both run on the host with what the branch's files say. A job sees LFS pointer files and empty submodule directories. Fetch them in a step, which runs in the container.
- The runner user, with an image of your own. The default images run jobs as `runner` with `sudo`, as GitHub does, but an image of your own runs as its own user, often root. Then a test that expects a `chmod 000` file to be unreadable passes on GitHub and fails here. Skip such tests when the effective user is root.
- Jobs that need a full VM. Mounting loop devices, systemd and some service containers do not work in Docker.
- GitHub's own services. Jobs get no `GITHUB_TOKEN` and no OIDC. `actions/cache` always misses, since the warm checkout per job does its work.
- `actions/upload-artifact` v7. act 0.2.89's artifact server refuses its requests (`unknown field "mime_type"`), so the step fails after five retries. Pin the step to v4 to v6, or leave the upload out of the workflows shipcheck runs.
- The event payload. act starts each workflow with a `workflow_dispatch` event that has no `repository`, so an action that reads the owner from the payload, such as gitleaks-action, fails.
- Docker inside a job. Jobs get no Docker socket, since a job with it controls the whole machine. Steps that run `docker build`, `docker compose` or Testcontainers fail. Docker actions and `services:` still work, since act starts them itself.

`workflows` takes whole workflows. If one job of a workflow cannot run under act, leave that workflow out of `workflows`, or move the job to its own workflow.

## Costs to know

- With `shard_tests` on, a failing run takes the sharded time plus a full serial run, since shipcheck reruns it whole before it reports the failure. A pytest step skipped by its own `if:` also makes the job run twice. Only a one-line pytest step in bash with no `;`, `&&`, pipes, redirects, `$` or pytest-xdist flags is sharded, one per job.

- Warm checkouts and cache volumes are not part of the result key. A broken toolchain in a volume breaks every run of that repository until you remove the volume with `docker volume rm`.
- shipcheck fetches each action once per job and reuses that copy without asking GitHub. An action pinned by tag or branch, such as `@v4`, stays at the commit it had then, even after its owner moves the tag. Pin actions by SHA, or run `shipcheck clean` to fetch them again.
- On macOS, Docker reaches the checkout through the VM's file sharing, which is slower for very large build directories than on Linux.
