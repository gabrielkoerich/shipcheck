+++
title = "Limits"
description = "What shipcheck does not catch, and what act cannot run."
weight = 70
+++

## What shipcheck does not catch

- **Weakened tests.** Tests are part of the PR. An agent can delete, skip or loosen a test and the run passes. Review test changes.
- **Differences from GitHub's runners.** act runs your steps in Docker images that resemble GitHub's runners but are not the same. GitHub still runs your workflows on `main` after each merge, which catches what slips through.
- **A determined cheat on the same machine.** shipcheck runs as your user. It stops an agent that claims a pass. It is not built to stop someone who sets out to forge one.

## What act cannot run

- **macOS and Windows jobs.** Docker runs only Linux jobs, and shipcheck never runs a job on the machine itself, so a workflow with macOS or Windows jobs fails with `no image for its runs-on label on this host`. Leave it to GitHub. The same message for a Linux label, such as `ubuntu-24.04` on act 0.2.89, means the label needs an image in `[act] platforms`.
- **The network, by default.** With `network = "none"`, the default, a job reaches nothing: no package downloads, no toolchain downloads from setup actions, and no artifacts passed between jobs. With `network = "internet"` all of that works, and a job can also reach this machine and the local network. Blocking those while keeping the internet is planned (docs/SECURITY.md in the repository).
- **Reusable workflows.** A job with `uses: ./.github/workflows/x.yml` or `uses: owner/repo/.github/workflows/x.yml@ref` fails the run, since act would run the called workflow's jobs without shipcheck's checks, Docker options included. Leave such a workflow out of `workflows` and let GitHub run it.
- **Git LFS files and submodules.** shipcheck checks out a commit on this machine with git-lfs and submodules turned off, since both run on the host with what the branch's files say. A job sees LFS pointer files and empty submodule directories. Fetch them in a step, which runs in the container.
- **Jobs that need a full VM.** Mounting loop devices, systemd and some service containers do not work in Docker.
- **GitHub's own services.** Jobs get no `GITHUB_TOKEN` and no OIDC. `actions/cache` always misses, since the warm checkout per job does its work.
- **Docker inside a job.** Jobs get no Docker socket, since a job with it controls the whole machine. Steps that run `docker build`, `docker compose` or Testcontainers fail. Docker actions and `services:` still work, since act starts them itself.

`workflows` takes whole workflows. If one job of a workflow cannot run under act, leave that workflow out of `workflows`, or move the job to its own workflow.

## Costs to know

- Warm checkouts and cache volumes are not part of the result key. A broken toolchain in a volume breaks every run of that repository until you remove the volume with `docker volume rm`.
- On macOS, Docker reaches the checkout through the VM's file sharing, which is slower for very large build directories than on Linux.
